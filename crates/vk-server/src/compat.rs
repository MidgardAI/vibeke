//! Herdr compatibility endpoint (07 §7.7, §8.3; 09 §6) — M5, slices 1 and 2.
//!
//! * **Compat listener** (`[compat.herdr] enabled = true`, off by default): a second socket
//!   speaking Herdr's wire protocol (`vk_compat::herdr::wire`), laid out like Herdr's: the
//!   default session at `$RUNTIME/herdr-compat/herdr.sock`, a named session at
//!   `$RUNTIME/herdr-compat/sessions/<name>/herdr.sock`. Never placed on Herdr's own socket
//!   path; removed on a clean stop. Caller identity comes from peer credentials with the native
//!   pane-scope rule.
//! * **Plugin brokers** ([`brokers`]): every plugin invocation and plugin pane gets a private
//!   0600 socket bound server-side to the plugin id and its legacy-grant digest, exported as
//!   `HERDR_SOCKET_PATH`. Brokers work whether or not the public listener is enabled,
//!   re-check the exact grant and the invocation's liveness on every request (and after an
//!   `events.wait` wakes), drop their accepted connections when they close, are re-issued after
//!   a server restart for live invocations, and outlive the invocation's process only for the
//!   manifest's long-running entrypoints (`[[startup]]` process groups, `[[panes]]`).
//! * **Cross-session calls** from a plugin (`herdr --session other …`): the shim obtains a
//!   single-use ticket from its own broker (`vibeke.invocation_ticket`) and the destination
//!   verifies it with the issuing session (`compat.invocation.verify`: broker open, invocation
//!   alive, same grant) before re-checking the grant itself. Environment values never carry
//!   plugin identity. Processes inside any Vibeke pane (any session) cannot switch sessions.
//! * **Method mapping**: Herdr methods are translated onto the native API (`api::dispatch`, so
//!   native authorization and events apply) and results are projected to Herdr shapes with
//!   Herdr-style ids (Vibeke handles); slice-2 methods live in [`ext`]. Known-but-unimplemented
//!   baseline methods return an explicit `unsupported` error; unknown methods answer
//!   `invalid_request`, as Herdr's typed request parse does.
//! * **Plugins**: `plugin.action.list/run` (native) and `plugin.action.invoke` (compat) run
//!   argv actions asynchronously with log records (output written by a [`capture`] helper into
//!   files bounded per invocation and tailed from them, so it survives a server restart,
//!   credentials redacted with `vk-redact`); `[[events]]` hooks fire on
//!   projected Herdr events; `[[startup]]` runs once per server activation. Nothing runs without
//!   a valid `herdr_legacy` grant (`vibeke plugin trust <id> --legacy`). Invocations and
//!   mutating broker calls are audited as metadata-only events (`plugin.invocation_started`,
//!   `plugin.invocation_finished`, `plugin.api_call` with `actor.kind = plugin`).

mod brokers;
pub mod capture;
mod ext;
mod gap;
mod limits;
mod rest;
pub mod trace;
mod views;

use crate::Server;
use crate::api::{self, Ctx, R, err, invalid};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use vk_compat::herdr::events::{self as hev, Projector};
use vk_compat::herdr::manifest::Manifest;
use vk_compat::herdr::registry::{self, Entry, PluginDirs, Registry, Status};
use vk_compat::herdr::wire::{self, WireError, typed};
use vk_compat::herdr::{self, inventory, launch, status};
use vk_proto::model::*;
use vk_proto::rpc::ErrorKind;

pub const METHODS: &[(&str, bool)] = &[
    ("plugin.list", false),
    ("plugin.action.list", false),
    ("plugin.action.run", true),
    ("plugin.log.list", false),
    ("compat.herdr.call", true),
    ("compat.invocation.verify", true),
    ("compat.status", false),
    ("plugin.link_handler.list", false),
    ("plugin.link.open", true),
    ("plugin.surface.close", true),
    ("compat.ui.state", false),
    ("plugin.registry.notify", true),
];

/// Log records kept per server (oldest dropped first); the per-plugin ring caps (bytes, lines,
/// records) come from `[plugins]` settings ([`limits`]).
const MAX_LOGS: usize = 100;

// ---- per-server state -------------------------------------------------------------------------

#[derive(Default)]
struct State {
    logs: Mutex<VecDeque<Value>>,
    next: AtomicU64,
    /// Live broker bindings by socket path.
    bindings: Mutex<HashMap<PathBuf, brokers::Live>>,
    /// When `notification.show` last delivered (Herdr's one-per-second limit).
    last_notification: Mutex<Option<std::time::Instant>>,
    /// Herdr's hook authority per pane (Vibeke id): the `(source, agent)` of the last
    /// accepted `pane.report_agent`, while its self-reported run lives.
    authority: Mutex<HashMap<String, (String, String)>>,
    /// Reported metadata (`pane.report_metadata`, `workspace.report_metadata`) by Vibeke id,
    /// and the plugin-set window title (`client.window_title.set`).
    meta: Mutex<ext::Meta>,
    /// Single-use cross-session tickets: ticket → (issuing broker, expiry).
    tickets: Mutex<HashMap<String, (PathBuf, std::time::Instant)>>,
    /// Running action/hook invocations per plugin (07 §7.7 concurrency limit).
    running: Mutex<HashMap<String, usize>>,
    /// Event-hook invocations waiting for a free slot, per plugin.
    queues: Mutex<HashMap<String, VecDeque<limits::Queued>>>,
    /// Plugin-provided agent views (`agent.view.set`).
    views: Mutex<views::Views>,
    /// Where the user's focus was last (Herdr keeps one focus without a client attached).
    focus_mem: Mutex<FocusMem>,
    /// Herdr ids (Vibeke handles) by Vibeke id, kept after the object is gone so close events
    /// still carry its Herdr id.
    handles: Mutex<HashMap<String, String>>,
}

/// The server-side focus memory: the workspace a client focused last, and per workspace its
/// last focused tab and pane (Vibeke ids). Plugin contexts and Herdr's "active" workspace, tab
/// and pane use it when no TUI client is attached (Herdr `current_plugin_context`).
#[derive(Debug, Clone, Default)]
struct FocusMem {
    workspace: Option<String>,
    tabs: HashMap<String, String>,
    panes: HashMap<String, String>,
}

impl FocusMem {
    fn record(&mut self, ws: &str, tab: Option<&str>, pane: Option<&str>) {
        self.workspace = Some(ws.to_string());
        if let Some(t) = tab {
            self.tabs.insert(ws.to_string(), t.to_string());
        }
        if let Some(p) = pane {
            self.panes.insert(ws.to_string(), p.to_string());
        }
    }
}

static STATES: LazyLock<Mutex<HashMap<String, Arc<State>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn state(server: &Server) -> Arc<State> {
    STATES
        .lock()
        .unwrap()
        .entry(server.boot_id.clone())
        .or_default()
        .clone()
}

/// Per-user plugin locations: the registry and plugin config dirs next to `config.toml`,
/// managed checkouts and plugin state under the state root (shared by all sessions).
pub fn plugin_dirs() -> PluginDirs {
    let cfg = vk_config::config_path();
    let cdir = cfg
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let state = crate::paths::state_root().join("plugins");
    PluginDirs {
        registry: cdir.join("plugins.json"),
        checkouts: state.join("checkouts"),
        config: cdir.join("plugins"),
        state: state.join("state"),
    }
}

/// `$RUNTIME/<session>/herdr-compat` (0700).
pub fn compat_root(server: &Server) -> PathBuf {
    server.paths.runtime.join("herdr-compat")
}

/// `$RUNTIME/herdr-compat`: the machine's Herdr-style socket root, shared by all sessions so
/// tools that discover Herdr sessions see them all (07 §8.3).
pub fn herdr_root(server: &Server) -> PathBuf {
    server
        .paths
        .runtime
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(crate::paths::runtime_root)
        .join("herdr-compat")
}

/// The public listener of this session: `<root>/herdr.sock` for `default`, else
/// `<root>/sessions/<name>/herdr.sock`.
pub fn listener_path(server: &Server) -> PathBuf {
    gap::session_listener(
        gap::configured_socket().as_deref(),
        &herdr_root(server),
        &server.opts.session,
    )
}

pub use ext::registry_op as herdr_registry_op;
pub use gap::{extend_pane_env, listener_for};

fn private_dir(p: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(p)?;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700))
}

/// Herdr's own socket locations, which the compat layer must never bind or remove.
pub fn is_herdr_owned(path: &Path) -> bool {
    let herdr = crate::paths::home().join(".config/herdr");
    path.starts_with(&herdr)
        || std::env::var_os("XDG_CONFIG_HOME")
            .is_some_and(|x| path.starts_with(PathBuf::from(x).join("herdr")))
}

/// Resolve `candidate` (an invocation's `HERDR_SOCKET_PATH`) to a broker socket this
/// installation registered, or refuse. The path must be exactly
/// `<runtime root>/<session>/herdr-compat/brokers/<name>.sock` with plain components (no `..`),
/// no component below the runtime root may be a symlink, the leaf must be a socket owned by
/// this user, and the session's broker registry (`brokers.json`) must list that very socket.
/// Returns the canonical socket path and its session. This is what keeps the shim from ever
/// talking to a live Herdr (or any other) socket.
pub fn registered_broker(candidate: &Path) -> Result<(PathBuf, String), String> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    use std::path::Component;
    let root = crate::paths::runtime_root();
    let root_c = root
        .canonicalize()
        .map_err(|e| format!("{}: {e}", root.display()))?;
    let rel = candidate
        .strip_prefix(&root)
        .or_else(|_| candidate.strip_prefix(&root_c))
        .map_err(|_| "not under Vibeke's runtime directory".to_string())?;
    let parts: Vec<&std::ffi::OsStr> = rel
        .components()
        .map(|c| match c {
            Component::Normal(s) => Ok(s),
            _ => Err("unexpected path component".to_string()),
        })
        .collect::<Result<_, _>>()?;
    let [session, compat, dir, name] = parts.as_slice() else {
        return Err("not a broker socket path".into());
    };
    let session = session.to_str().unwrap_or_default();
    if !herdr::valid_session_name(session)
        || *compat != "herdr-compat"
        || *dir != "brokers"
        || !name.to_str().is_some_and(|n| n.ends_with(".sock"))
    {
        return Err("not a broker socket path".into());
    }
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    let mut cur = root_c.clone();
    for (i, part) in parts.iter().enumerate() {
        cur.push(part);
        let m = std::fs::symlink_metadata(&cur).map_err(|e| format!("{}: {e}", cur.display()))?;
        let ft = m.file_type();
        let ok = if i + 1 == parts.len() {
            ft.is_socket()
        } else {
            ft.is_dir()
        };
        if ft.is_symlink() || !ok || m.uid() != uid {
            return Err(format!("{} is not a Vibeke broker", cur.display()));
        }
    }
    let registry = root_c.join(session).join("herdr-compat/brokers.json");
    if std::fs::symlink_metadata(&registry).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err("broker registry is a symlink".into());
    }
    let bytes = match std::fs::read(&registry) {
        Ok(b) => b,
        // Inside a restricted (sandboxed) invocation the runtime dir and its broker registry
        // are hidden on purpose (the registry names every other invocation's broker). There the
        // OS sandbox itself allows connecting to the invocation's own broker socket only, so the
        // structural checks above are what remains to be verified.
        Err(e)
            if std::env::var("VIBEKE_ISOLATION").as_deref() == Ok("sandbox")
                && matches!(
                    e.kind(),
                    std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
                ) =>
        {
            return Ok((cur, session.to_string()));
        }
        Err(_) => Vec::new(),
    };
    let list: Vec<Value> = serde_json::from_slice(&bytes).unwrap_or_default();
    let registered = list.iter().any(|b| {
        b["path"]
            .as_str()
            .and_then(|p| Path::new(p).canonicalize().ok())
            .is_some_and(|p| p == cur)
    });
    if !registered {
        return Err("not a registered Vibeke broker".into());
    }
    Ok((cur, session.to_string()))
}

/// The private `herdr` launcher for plugin invocations: `<compat>/bin/herdr` → Vibeke binary.
fn launcher(server: &Server) -> PathBuf {
    let dir = compat_root(server).join("bin");
    let link = dir.join("herdr");
    if private_dir(&dir).is_ok() {
        let current = std::fs::read_link(&link).ok();
        if current.as_deref() != Some(server.opts.bin.as_path()) {
            let _ = std::fs::remove_file(&link);
            let _ = std::os::unix::fs::symlink(&server.opts.bin, &link);
        }
    }
    link
}

fn compat_enabled() -> bool {
    vk_config::Config::load(vk_config::config_path())
        .map(|(c, _)| c.compat.herdr.enabled || c.compat.herdr_socket)
        .unwrap_or(false)
}

// ---- startup ----------------------------------------------------------------------------------

/// Called once from `run::serve` after recovery: broker re-issue, event-hook dispatcher,
/// startup hooks, and the public listener when enabled.
pub fn start(server: &Arc<Server>) {
    load_logs(server);
    let (ok, dropped) = brokers::recover(server);
    if ok + dropped > 0 {
        tracing::info!(recovered = ok, dropped, "herdr plugin brokers");
    }
    let s = server.clone();
    tokio::spawn(async move { hook_dispatcher(s).await });
    let s = server.clone();
    tokio::spawn(async move {
        // After session restore and API readiness (07 §7.7).
        tokio::time::sleep(Duration::from_millis(200)).await;
        run_startup_hooks(&s);
    });
    if compat_enabled() {
        match bind_listener(server) {
            Ok(l) => {
                let s = server.clone();
                tokio::spawn(async move { accept_public(s, l).await });
                tracing::info!(socket = %listener_path(server).display(), "herdr compat listener");
            }
            Err(e) => tracing::warn!(error = %e, "herdr compat listener not started"),
        }
    }
}

fn bind_listener(server: &Arc<Server>) -> std::io::Result<UnixListener> {
    let path = listener_path(server);
    // Binding in Herdr's own directory needs the explicit `compat.herdr_socket_path`; a live
    // socket there is still never replaced (`bind_socket`).
    let custom = gap::configured_socket().is_some();
    if is_herdr_owned(&path) && !custom {
        return Err(std::io::Error::other(
            "refusing to bind inside Herdr's config directory",
        ));
    }
    if custom {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
    } else {
        let root = herdr_root(server);
        private_dir(&root)?;
        if let Some(dir) = path.parent()
            && dir != root
        {
            private_dir(&root.join("sessions"))?;
            private_dir(dir)?;
        }
    }
    let l = bind_socket(&path)?;
    // Remove the socket on a clean stop (clients treat presence as liveness): on SIGTERM/SIGINT
    // and on `server.stop`.
    let p = path.clone();
    let srv = server.clone();
    tokio::spawn(async move {
        use tokio::signal::unix::{SignalKind, signal};
        let (Ok(mut term), Ok(mut int)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        ) else {
            return;
        };
        tokio::select! {
            _ = term.recv() => {},
            _ = int.recv() => {},
            _ = srv.shutdown.notified() => {},
        }
        let _ = std::fs::remove_file(&p);
        if let Some(dir) = p.parent()
            && dir != herdr_root(&srv)
            && !custom
        {
            let _ = std::fs::remove_dir(dir);
        }
    });
    Ok(l)
}

fn bind_socket(path: &Path) -> std::io::Result<UnixListener> {
    use std::os::unix::fs::PermissionsExt;
    if path.exists() {
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            return Err(std::io::Error::other(format!(
                "{} is in use",
                path.display()
            )));
        }
        let _ = std::fs::remove_file(path);
    }
    let l = std::os::unix::net::UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    l.set_nonblocking(true)?;
    UnixListener::from_std(l)
}

fn same_uid(s: &UnixStream) -> bool {
    // SAFETY: getuid has no preconditions.
    s.peer_cred()
        .map(|c| c.uid() == unsafe { libc::getuid() })
        .unwrap_or(false)
}

async fn accept_public(server: Arc<Server>, l: UnixListener) {
    loop {
        let Ok((stream, _)) = l.accept().await else {
            return;
        };
        if !same_uid(&stream) {
            continue;
        }
        let pid = stream.peer_cred().ok().and_then(|c| c.pid());
        let scope = crate::run::ancestry_pane(&server, pid);
        let caller = Caller {
            ctx: Ctx {
                client_id: format!("herdr-{}", &crate::core::ulid()[20..]),
                kind: if scope.is_some() { "agent" } else { "herdr" }.into(),
                pane_scope: scope,
                remote: false,
            },
            plugin: None,
            default_pane: None,
            invocation: None,
            broker: None,
            grant_id: None,
            cross_session: false,
            sandboxed: false,
            peer_bound: false,
            token: None,
        };
        let s = server.clone();
        tokio::spawn(async move { serve_wire(s, stream, caller).await });
    }
}

// ---- callers and the wire loop ----------------------------------------------------------------

/// Identity of one compat connection.
#[derive(Debug, Clone)]
pub struct Caller {
    pub ctx: Ctx,
    /// `(plugin id, grant digest)` for broker connections.
    pub plugin: Option<(String, String)>,
    /// The invocation's pane, used when a request omits `pane_id`.
    pub default_pane: Option<String>,
    /// The invocation's log id (audit correlation), for broker connections.
    pub invocation: Option<String>,
    /// The broker this connection came through: its binding must stay open and its invocation
    /// alive for every request.
    pub broker: Option<PathBuf>,
    /// The exact grant the invocation runs under.
    pub grant_id: Option<String>,
    /// A plugin invocation of another session, authenticated by a verified ticket.
    pub cross_session: bool,
    /// A restricted (sandboxed) invocation: only [`sandbox_allows`] methods are served.
    pub sandboxed: bool,
    /// Broker connections: the connecting process belongs to the invocation (its process
    /// tree, process group or pane).
    pub peer_bound: bool,
    /// Broker connections: the invocation's secret, accepted instead of `peer_bound`.
    pub token: Option<String>,
}

impl Caller {
    /// A user-level caller (CLI shim over the native socket, tests).
    pub fn user(ctx: Ctx) -> Self {
        Caller {
            ctx,
            plugin: None,
            default_pane: None,
            invocation: None,
            broker: None,
            grant_id: None,
            cross_session: false,
            sandboxed: false,
            peer_bound: false,
            token: None,
        }
    }
}

/// What a restricted (sandboxed) plugin may call through its broker (07 §7.7, 09 §6): reading
/// session state (workspaces, tabs, panes, agents, layout — not terminal contents or host
/// process details), waiting for events, its own agent views, notifications, closing its own
/// popup, its own log records and its own actions (which run restricted too). Everything that
/// executes on the host or types into a pane (`pane.run`, `pane.send_*`, `agent.send`,
/// `agent.prompt`, `agent.start`, `tab/pane.create`/`split` with commands), changes layout,
/// workspaces, metadata, the registry or worktrees, runs git on the host, reads pane output, or
/// acts for another plugin is refused. Host-mode plugins are not affected.
pub fn sandbox_allows(method: &str) -> bool {
    matches!(
        method,
        "ping"
            | "api.schema"
            | "session.snapshot"
            | "workspace.list"
            | "workspace.get"
            | "tab.list"
            | "tab.get"
            | "pane.list"
            | "pane.get"
            | "pane.current"
            | "agent.list"
            | "agent.get"
            | "layout.export"
            | "pane.layout"
            | "pane.neighbor"
            | "pane.edges"
            | "events.subscribe"
            | "events.wait"
            | "plugin.list"
            | "plugin.action.list"
            | "plugin.log.list"
            | "plugin.action.invoke"
            | "agent.view.set"
            | "agent.view.clear"
            | "notification.show"
            | "popup.close"
    )
}

fn sandbox_denied(method: &str) -> WireError {
    WireError::new(
        "permission_denied",
        format!(
            "{method} is not available to a plugin in restricted (sandboxed) mode; it may read session state, wait for events, set its own agent views, notify and run its own actions (use isolate = \"host\" for full legacy access)"
        ),
    )
}

/// Constant-time comparison for the broker token.
fn same_secret(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

/// Herdr methods that only read; every other method a broker calls is audited (09 §6).
fn read_only(method: &str) -> bool {
    matches!(
        method,
        "ping"
            | "api.schema"
            | "session.snapshot"
            | "workspace.list"
            | "workspace.get"
            | "tab.list"
            | "tab.get"
            | "pane.list"
            | "pane.get"
            | "pane.current"
            | "pane.neighbor"
            | "pane.edges"
            | "pane.read"
            | "pane.wait_for_output"
            | "pane.process_info"
            | "agent.list"
            | "agent.get"
            | "agent.read"
            | "agent.wait"
            | "worktree.list"
            | "worktree.repo_root"
            | "layout.export"
            | "pane.layout"
            | "events.subscribe"
            | "events.wait"
            | "plugin.list"
            | "plugin.action.list"
            | "plugin.log.list"
            | "pane.selection.read"
            | "pane.link.resolve"
            | "server.agent_manifests"
    )
}

/// Commit one metadata-only audit event.
fn audit(server: &Server, kind: &str, subject: Value, actor: Value, data: Value) {
    let mut c = server.core.lock().unwrap();
    let mut tx = crate::core::Tx::new();
    tx.event_by(kind, subject, actor, data);
    if let Err(e) = server.commit(&mut c, tx) {
        tracing::warn!(error = %e, kind, "audit event not recorded");
    }
}

/// Audit a mutating call made with a plugin's broker identity: method and outcome only, never
/// parameters or text (09 §6 "Audit compat mutations with the plugin identity").
fn audit_call(server: &Server, caller: &Caller, method: &str, outcome: Result<(), &str>) {
    let Some((plugin, _)) = &caller.plugin else {
        return;
    };
    if read_only(method) {
        return;
    }
    audit(
        server,
        "plugin.api_call",
        json!({"plugin": plugin}),
        json!({"kind": "plugin", "id": plugin, "invocation": caller.invocation}),
        json!({"method": method, "ok": outcome.is_ok(), "error_code": outcome.err()}),
    );
}

/// A plugin caller's authority must still hold: registered, enabled and trusted under the very
/// grant (digest and grant id) the invocation started with, and — for broker connections — the
/// broker still open and its invocation alive.
fn check_broker(server: &Server, caller: &Caller) -> Result<(), WireError> {
    let Some((id, digest)) = &caller.plugin else {
        return Ok(());
    };
    let grant_id = caller.grant_id.as_deref().unwrap_or_default();
    if !brokers::grant_ok(id, digest, grant_id) {
        return Err(WireError::new(
            "permission_denied",
            format!("plugin {id} is no longer trusted or enabled"),
        ));
    }
    if let Some(b) = &caller.broker
        && brokers::live(server, b).is_none_or(|x| x.grant_id != grant_id)
    {
        return Err(WireError::new(
            "permission_denied",
            "this plugin invocation has ended; its broker is closed",
        ));
    }
    if caller.broker.is_none() && !caller.cross_session {
        return Err(WireError::new(
            "permission_denied",
            "plugin identity without an invocation",
        ));
    }
    Ok(())
}

/// Trace one broker result for a plugin caller (spec 18 §9; no-op unless tracing is on).
fn trace_result(
    caller: &Caller,
    transport: &str,
    method: &str,
    params: Option<&Value>,
    outcome: Result<(), &str>,
) {
    if trace::settings().is_none() {
        return;
    }
    let Some((plugin, _)) = &caller.plugin else {
        return;
    };
    trace::write(&trace::Line {
        plugin,
        invocation: caller.invocation.as_deref(),
        transport,
        method,
        ok: outcome.is_ok(),
        code: outcome.err(),
        params,
        ..Default::default()
    });
}

async fn write_line(w: &mut (impl AsyncWriteExt + Unpin), line: &str) -> std::io::Result<()> {
    w.write_all(line.as_bytes()).await?;
    w.write_all(b"\n").await?;
    w.flush().await
}

/// One Herdr connection: a single request (closed after the response), or a subscription
/// stream for `events.subscribe`.
pub async fn serve_wire(server: Arc<Server>, stream: UnixStream, caller: Caller) {
    let (rd, mut wr) = stream.into_split();
    let mut rd = BufReader::new(rd).take(wire::MAX_LINE as u64 + 1);
    let mut buf = Vec::new();
    match rd.read_until(b'\n', &mut buf).await {
        Ok(0) | Err(_) => return,
        Ok(_) => {}
    }
    // Test-only trace (spec 18 §9): `cli` for the `herdr` launcher, `socket` for raw clients.
    let transport = if trace::settings().is_some() && trace::from_shim(&buf) {
        "cli"
    } else {
        "socket"
    };
    // Herdr closes the connection without an answer for a blank, non-UTF-8 or over-long line.
    if !wire::answers(&buf) {
        trace_result(&caller, transport, "", None, Err("invalid_request"));
        return;
    }
    let req = match wire::parse_request(&buf) {
        Ok(r) => r,
        Err((id, e)) => {
            trace_result(&caller, transport, "", None, Err(e.code.as_str()));
            let _ = write_line(&mut wr, &wire::err_line(&id, &e)).await;
            return;
        }
    };
    let traced = |outcome: Result<(), &str>| {
        trace_result(&caller, transport, &req.method, Some(&req.params), outcome)
    };
    // A broker serves its own invocation only: a process of it, or one holding its token.
    if caller.broker.is_some() && !caller.peer_bound {
        let ok = matches!(
            (&caller.token, &req.token),
            (Some(want), Some(got)) if same_secret(want, got)
        );
        if !ok {
            let e = WireError::new(
                "permission_denied",
                "this broker belongs to another plugin invocation; the connecting process is not part of it",
            );
            traced(Err(e.code.as_str()));
            let _ = write_line(&mut wr, &wire::err_line(&req.id, &e)).await;
            return;
        }
    }
    if let Err(e) = check_broker(&server, &caller) {
        traced(Err(e.code.as_str()));
        let _ = write_line(&mut wr, &wire::err_line(&req.id, &e)).await;
        return;
    }
    if caller.sandboxed && !sandbox_allows(&req.method) {
        let e = sandbox_denied(&req.method);
        audit_call(&server, &caller, &req.method, Err(e.code.as_str()));
        traced(Err(e.code.as_str()));
        let _ = write_line(&mut wr, &wire::err_line(&req.id, &e)).await;
        return;
    }
    if req.method == "events.subscribe" {
        let subs = match hev::parse_subscriptions(&req.params) {
            Ok(s) => s,
            Err((code, msg)) => {
                traced(Err(code));
                let _ = write_line(
                    &mut wr,
                    &wire::err_line(&req.id, &WireError::new(code, msg)),
                )
                .await;
                return;
            }
        };
        // Herdr's output watchers: the pane must exist and the regex compile before the
        // subscription starts.
        let mut watchers = match output_watchers(&server, &subs) {
            Ok(w) => w,
            Err(e) => {
                traced(Err(e.code.as_str()));
                let _ = write_line(&mut wr, &wire::err_line(&req.id, &e)).await;
                return;
            }
        };
        let mut rx = server.events.subscribe();
        let mut proj = seeded_projector(&server);
        traced(Ok(()));
        let ack = typed("subscription_started", json!({}));
        if write_line(&mut wr, &wire::ok_line(&req.id, ack))
            .await
            .is_err()
        {
            return;
        }
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        loop {
            let ev = tokio::select! {
                r = rx.recv() => match r {
                    Ok(e) => Some(e),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return,
                },
                _ = tick.tick(), if !watchers.is_empty() => None,
            };
            let Some(ev) = ev else {
                for w in watchers.iter_mut() {
                    let Some(data) = poll_watcher(&server, &caller, w).await else {
                        continue;
                    };
                    if check_broker(&server, &caller).is_err() {
                        return;
                    }
                    let line = hev::stream_event("pane.output_matched", data).to_string();
                    if write_line(&mut wr, &line).await.is_err() {
                        return;
                    }
                }
                continue;
            };
            for (name, pane, data) in project_event(&server, &mut proj, &ev) {
                if !subs.iter().any(|s| hev::matches(s, name, pane.as_deref())) {
                    continue;
                }
                if check_broker(&server, &caller).is_err() {
                    return;
                }
                // Herdr's `EventEnvelope`, or for the pane-scoped subscriptions its
                // `SubscriptionEventEnvelope` (dotted name, no `type`).
                if write_line(&mut wr, &hev::stream_event(name, data).to_string())
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
    }
    // Herdr stops a wait when its client goes away (`should_stop_connection`): a wait here
    // ends without an answer once the peer closes the connection.
    let waits = matches!(
        req.method.as_str(),
        "events.wait" | "pane.wait_for_output" | "agent.wait"
    );
    let r = if waits && rd.limit() > 0 {
        let fut = call(&server, &caller, &req.method, &req.params);
        tokio::pin!(fut);
        let mut probe = [0u8; 64];
        loop {
            tokio::select! {
                r = &mut fut => break r,
                n = rd.read(&mut probe) => match n {
                    Ok(0) | Err(_) => {
                        traced(Err("client_disconnected"));
                        return;
                    }
                    // Bytes after the request line are ignored, as in Herdr.
                    Ok(_) => {}
                },
            }
        }
    } else {
        call(&server, &caller, &req.method, &req.params).await
    };
    audit_call(
        &server,
        &caller,
        &req.method,
        r.as_ref().map(|_| ()).map_err(|e| e.code.as_str()),
    );
    traced(r.as_ref().map(|_| ()).map_err(|e| e.code.as_str()));
    let line = match r {
        Ok(v) => wire::ok_line(&req.id, v),
        Err(e) => wire::err_line(&req.id, &e),
    };
    let _ = write_line(&mut wr, &line).await;
}

// ---- projection -------------------------------------------------------------------------------

/// A consistent copy of the model parts the projection needs.
struct Snap {
    ws: Vec<Workspace>,
    tabs: Vec<Tab>,
    panes: Vec<Pane>,
    pane_run: HashMap<String, AgentRun>,
    blocked: HashSet<String>,
    /// The most recently active TUI client's focus (empty without one).
    focus: ClientFocus,
    /// The server-side focus memory ([`FocusMem`]).
    last: FocusMem,
    meta: ext::Meta,
}

fn snap(server: &Server) -> Snap {
    let mut focus = crate::notify::recent_client(server)
        .map(|c| server.client_focus(&c))
        .unwrap_or_default();
    let meta = state(server).meta.lock().unwrap().clone();
    // Popups keep the focus context underneath them (07 §7.7).
    if let Some(p) = focus.pane.clone()
        && is_popup_pane(server, &meta, &p)
    {
        focus.pane = meta
            .plugin_panes
            .get(&p)
            .and_then(|pp| pp.prev_focus.clone());
    }
    let st = state(server);
    let last = {
        let mut m = st.focus_mem.lock().unwrap();
        if let Some(w) = focus.workspace.as_deref() {
            m.record(w, focus.tab.as_deref(), focus.pane.as_deref());
        }
        m.clone()
    };
    let sn = server.with_core(|c| {
        let mut ws = c.model.workspaces.clone();
        ws.sort_by(|a, b| a.order.total_cmp(&b.order));
        let mut tabs = c.model.tabs.clone();
        tabs.sort_by(|a, b| a.order.total_cmp(&b.order));
        let pane_run = c
            .model
            .panes
            .iter()
            .filter_map(|p| c.run_for_pane(&p.id).map(|r| (p.id.clone(), r.clone())))
            .collect();
        let blocked = c
            .model
            .interactions
            .iter()
            .filter(|i| i.status == InteractionStatus::Open)
            .map(|i| i.run.clone())
            .collect();
        Snap {
            ws,
            tabs,
            panes: c
                .model
                .panes
                .iter()
                .filter(|p| !p.plugin_surface().is_some_and(|s| s.is_popup()))
                .cloned()
                .collect(),
            pane_run,
            blocked,
            focus,
            last,
            meta,
        }
    });
    {
        let mut h = st.handles.lock().unwrap();
        if h.len() > 8192 {
            h.clear();
        }
        let ids = sn.ws.iter().map(|w| (&w.id, &w.handle));
        let ids = ids.chain(sn.tabs.iter().map(|t| (&t.id, &t.handle)));
        let ids = ids.chain(sn.panes.iter().map(|p| (&p.id, &p.handle)));
        for (id, handle) in ids {
            h.insert(id.clone(), handle.clone());
        }
    }
    sn
}

/// The Herdr id of a Vibeke object, also after it closed.
fn known_handle(server: &Server, id: &str) -> Option<String> {
    state(server).handles.lock().unwrap().get(id).cloned()
}

/// A plugin popup's pane (live, by its tag, or one that already closed).
fn is_popup_pane(server: &Server, meta: &ext::Meta, pane: &str) -> bool {
    meta.popups.contains(pane)
        || server.with_core(|c| {
            c.pane(pane)
                .and_then(|p| p.plugin_surface())
                .is_some_and(|s| s.is_popup())
        })
}

/// A client's viewport of `pane` moved in its scrollback (`ClientFrame::ScrollView`): remember
/// the offset (`scroll` in pane records) and emit `pane.scroll_changed` when it changed.
pub fn scroll_changed(server: &Server, client: &str, pane: &str, offset: u32, total: u32) {
    let Some(p) = server.with_core(|c| c.pane(pane).cloned()) else {
        return;
    };
    {
        let st = state(server);
        let mut meta = st.meta.lock().unwrap();
        if meta.scroll.get(pane) == Some(&offset)
            || (offset == 0 && !meta.scroll.contains_key(pane))
        {
            return;
        }
        if offset == 0 {
            meta.scroll.remove(pane);
        } else {
            meta.scroll.insert(pane.to_string(), offset);
        }
    }
    let mut c = server.core.lock().unwrap();
    let mut tx = crate::core::Tx::new();
    tx.event(
        "pane.scroll_changed",
        crate::core::subject_pane(&p),
        json!({"offset": offset, "total": total, "client": client}),
    );
    let _ = server.commit(&mut c, tx);
}

impl Snap {
    fn ws(&self, id: &str) -> Option<&Workspace> {
        self.ws.iter().find(|w| w.id == id || w.handle == id)
    }
    fn tab(&self, id: &str) -> Option<&Tab> {
        self.tabs.iter().find(|t| t.id == id || t.handle == id)
    }
    fn pane(&self, id: &str) -> Option<&Pane> {
        self.panes.iter().find(|p| p.id == id || p.handle == id)
    }
    fn ws_handle(&self, id: &str) -> Option<String> {
        self.ws(id).map(|w| w.handle.clone())
    }
    fn tab_handle(&self, id: &str) -> Option<String> {
        self.tab(id).map(|t| t.handle.clone())
    }
    fn run_status(&self, r: &AgentRun) -> &'static str {
        let unseen = self
            .pane(&r.pane)
            .is_some_and(|p| (p.unread || p.marked_unread) && r.turns_completed > 0);
        status::agent_status(
            r.execution.value.as_str(),
            self.blocked.contains(&r.id),
            unseen,
        )
    }
    fn pane_status(&self, pane: &str) -> Option<&'static str> {
        self.pane_run.get(pane).map(|r| self.run_status(r))
    }
    /// Herdr's active workspace: the most recent client's, else the one a client focused last,
    /// else the first.
    fn active_ws(&self) -> Option<&Workspace> {
        self.focus
            .workspace
            .as_deref()
            .and_then(|w| self.ws(w))
            .or_else(|| self.last.workspace.as_deref().and_then(|w| self.ws(w)))
            .or(self.ws.first())
    }
    /// A workspace's active tab (same fallbacks as [`Self::active_ws`]).
    fn active_tab(&self, w: &Workspace) -> Option<&Tab> {
        let mine = |t: &&Tab| t.workspace == w.id;
        self.focus
            .tab
            .as_deref()
            .and_then(|t| self.tab(t))
            .filter(mine)
            .or_else(|| {
                self.last
                    .tabs
                    .get(&w.id)
                    .and_then(|t| self.tab(t))
                    .filter(mine)
            })
            .or_else(|| self.tabs.iter().find(|t| t.workspace == w.id))
    }
    /// A tab's focused pane: the client's, the remembered one, the tab's own, else its first.
    fn tab_focus(&self, t: &Tab) -> Option<&Pane> {
        let mine = |p: &&Pane| p.tab == t.id;
        self.focus
            .pane
            .as_deref()
            .and_then(|p| self.pane(p))
            .filter(mine)
            .or_else(|| {
                self.last
                    .panes
                    .get(&t.workspace)
                    .and_then(|p| self.pane(p))
                    .filter(mine)
            })
            .or_else(|| {
                t.focused_pane
                    .as_deref()
                    .and_then(|p| self.pane(p))
                    .filter(mine)
            })
            .or_else(|| self.panes.iter().find(|p| p.tab == t.id && !p.exited))
    }
    fn tab_is_active(&self, t: &Tab) -> bool {
        self.active_ws().is_some_and(|w| w.id == t.workspace)
            && self
                .ws(&t.workspace)
                .and_then(|w| self.active_tab(w))
                .is_some_and(|a| a.id == t.id)
    }
    fn pane_is_focused(&self, p: &Pane) -> bool {
        self.tab(&p.tab)
            .filter(|t| self.tab_is_active(t))
            .and_then(|t| self.tab_focus(t))
            .is_some_and(|f| f.id == p.id)
    }
    /// Herdr's tab label: the custom name, else the tab's position in its workspace.
    fn tab_label(&self, t: &Tab) -> String {
        t.title
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                let i = self
                    .tabs
                    .iter()
                    .filter(|x| x.workspace == t.workspace)
                    .position(|x| x.id == t.id)
                    .unwrap_or(0);
                (i + 1).to_string()
            })
    }
    /// Herdr `WorkspaceInfo`, plus Vibeke's `cwd` and reported `metadata`.
    fn ws_json(&self, w: &Workspace) -> Value {
        let idx = self.ws.iter().position(|x| x.id == w.id).unwrap_or(0);
        let tabs = self.tabs.iter().filter(|t| t.workspace == w.id).count();
        let statuses: Vec<&str> = self
            .panes
            .iter()
            .filter(|p| p.workspace == w.id)
            .filter_map(|p| self.pane_status(&p.id))
            .collect();
        let mut v = Map::new();
        v.insert("workspace_id".into(), json!(w.handle));
        v.insert("number".into(), json!(idx + 1));
        v.insert("label".into(), json!(w.display_name()));
        v.insert(
            "focused".into(),
            json!(self.active_ws().is_some_and(|a| a.id == w.id)),
        );
        v.insert(
            "pane_count".into(),
            json!(self.panes.iter().filter(|p| p.workspace == w.id).count()),
        );
        v.insert("tab_count".into(), json!(tabs));
        v.insert(
            "active_tab_id".into(),
            json!(self.active_tab(w).map(|t| t.handle.clone())),
        );
        v.insert(
            "agent_status".into(),
            json!(status::most_urgent(statuses).unwrap_or("unknown")),
        );
        let meta = self.meta.workspaces.get(&w.id);
        if let Some(t) = meta.and_then(|m| tokens(m.get("tokens"))) {
            v.insert("tokens".into(), t);
        }
        if let Some(wt) = git_space(&w.root_path) {
            v.insert("worktree".into(), wt);
        }
        v.insert("cwd".into(), json!(w.root_path));
        if let Some(m) = meta {
            v.insert("metadata".into(), json!(m));
        }
        Value::Object(v)
    }
    /// Herdr `TabInfo`, plus Vibeke's `focused_pane_id`.
    fn tab_json(&self, t: &Tab) -> Value {
        let statuses: Vec<&str> = self
            .panes
            .iter()
            .filter(|p| p.tab == t.id)
            .filter_map(|p| self.pane_status(&p.id))
            .collect();
        json!({
            "tab_id": t.handle,
            "workspace_id": self.ws_handle(&t.workspace),
            "number": t.number,
            "label": self.tab_label(t),
            "focused": self.tab_is_active(t),
            "pane_count": self.panes.iter().filter(|p| p.tab == t.id).count(),
            "agent_status": status::most_urgent(statuses).unwrap_or("unknown"),
            "focused_pane_id": t.focused_pane.as_deref().and_then(|p| self.pane(p)).map(|p| p.handle.clone()),
        })
    }
    /// Herdr `PaneInfo` (absent optional fields left out, as Herdr does), plus Vibeke's
    /// reported `metadata`. `title`, `display_agent`, `state_labels` and `tokens` come from
    /// `pane.report_metadata`; `scroll` is the most recent client's viewport offset over the
    /// pane's scrollback.
    fn pane_json(&self, server: &Server, p: &Pane) -> Value {
        let run = self.pane_run.get(&p.id);
        let cwd = server.pane_cwd(&p.id).or_else(|| p.cwd.clone());
        let rt = server.pane_rt(&p.id);
        let rev = rt.as_ref().map(|r| r.rev()).unwrap_or(0);
        let (terminal_title, scroll) = match &rt {
            Some(rt) => {
                let sc = rt.screen.lock().unwrap();
                let title = sc.engine.title();
                // The offset is the client's view; the server's own scrollback may be
                // shorter (a client keeps more), so the maximum is at least the offset.
                let offset = u64::from(self.meta.scroll.get(&p.id).copied().unwrap_or(0));
                let max = (sc.engine.history_len() as u64).max(offset);
                (
                    (!title.is_empty()).then_some(title),
                    Some(json!({
                        "offset_from_bottom": offset,
                        "max_offset_from_bottom": max,
                        "viewport_rows": sc.engine.rows(),
                    })),
                )
            }
            None => (None, None),
        };
        let session = run.and_then(|r| {
            r.harness_session_id
                .as_ref()
                .map(|v| ("id", v.clone()))
                .or_else(|| r.transcript_path.as_ref().map(|v| ("path", v.clone())))
                .map(|(kind, value)| {
                    json!({"source": "vibeke", "agent": status::herdr_agent_name(&r.harness), "kind": kind, "value": value})
                })
        });
        let meta = self.meta.panes.get(&p.id);
        let ms = |k: &str| {
            meta.and_then(|m| m.get(k))
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        let mut v = Map::new();
        let opt = |v: &mut Map<String, Value>, k: &str, x: Option<Value>| {
            if let Some(x) = x {
                v.insert(k.into(), x);
            }
        };
        v.insert("pane_id".into(), json!(p.handle));
        v.insert("terminal_id".into(), json!(p.handle));
        v.insert("workspace_id".into(), json!(self.ws_handle(&p.workspace)));
        v.insert("tab_id".into(), json!(self.tab_handle(&p.tab)));
        v.insert("focused".into(), json!(self.pane_is_focused(p)));
        opt(&mut v, "cwd", cwd.clone().map(Value::String));
        opt(&mut v, "foreground_cwd", cwd.map(Value::String));
        opt(&mut v, "label", p.title.clone().map(Value::String));
        opt(
            &mut v,
            "agent",
            run.map(|r| json!(status::herdr_agent_name(&r.harness))),
        );
        opt(&mut v, "title", ms("title").map(Value::String));
        opt(&mut v, "terminal_title", terminal_title.map(Value::String));
        opt(
            &mut v,
            "display_agent",
            ms("display_agent").map(Value::String),
        );
        v.insert(
            "agent_status".into(),
            json!(run.map(|r| self.run_status(r)).unwrap_or("unknown")),
        );
        opt(
            &mut v,
            "state_labels",
            meta.and_then(|m| tokens(m.get("state_labels"))),
        );
        opt(&mut v, "tokens", meta.and_then(|m| tokens(m.get("tokens"))));
        opt(&mut v, "agent_session", session);
        opt(&mut v, "scroll", scroll);
        v.insert("revision".into(), json!(rev));
        if let Some(m) = meta {
            v.insert("metadata".into(), json!(m));
        }
        Value::Object(v)
    }
    fn agent_json(&self, server: &Server, r: &AgentRun) -> Value {
        json!({
            "agent_id": r.handle,
            "name": r.name,
            "agent": status::herdr_agent_name(&r.harness),
            "agent_status": self.run_status(r),
            "pane_id": self.pane(&r.pane).map(|p| p.handle.clone()),
            "pane": self.pane(&r.pane).map(|p| self.pane_json(server, p)),
        })
    }
    /// Herdr's focused workspace, tab and pane ids (session-wide, see [`Self::active_ws`]).
    fn focused(&self) -> (Option<String>, Option<String>, Option<String>) {
        let w = self.active_ws();
        let t = w.and_then(|w| self.active_tab(w));
        let p = t.and_then(|t| self.tab_focus(t));
        (
            w.map(|w| w.handle.clone()),
            t.map(|t| t.handle.clone()),
            p.map(|p| p.handle.clone()),
        )
    }

    // ---- plugin invocation contexts (Herdr `src/app/api/plugins/context.rs`) ----------------

    /// Herdr `plugin_context_from_parts`.
    fn ctx_parts(
        &self,
        server: &Server,
        w: &Workspace,
        t: Option<&Tab>,
        p: Option<&Pane>,
        correlation: &str,
    ) -> InvokeContext {
        let pane_cwd = p.and_then(|x| server.pane_cwd(&x.id).or_else(|| x.cwd.clone()));
        InvokeContext {
            workspace_id: Some(w.handle.clone()),
            workspace_label: Some(w.display_name().to_string()),
            workspace_cwd: pane_cwd.clone().or_else(|| Some(w.root_path.clone())),
            worktree: git_space(&w.root_path),
            tab_id: t.map(|t| t.handle.clone()),
            tab_label: t.map(|t| self.tab_label(t)),
            focused_pane_id: p.map(|x| x.handle.clone()),
            focused_pane_cwd: pane_cwd,
            focused_pane_agent: p
                .and_then(|x| self.pane_run.get(&x.id))
                .map(|r| status::herdr_agent_name(&r.harness).to_string()),
            focused_pane_status: p
                .map(|x| self.pane_status(&x.id).unwrap_or("unknown").to_string()),
            ..InvokeContext::empty(correlation)
        }
    }
    /// Herdr `plugin_context_for_workspace`: its active tab and that tab's focused pane.
    fn ctx_for_ws(&self, server: &Server, w: &Workspace, correlation: &str) -> InvokeContext {
        let t = self.active_tab(w);
        let p = t.and_then(|t| self.tab_focus(t));
        self.ctx_parts(server, w, t, p, correlation)
    }
    /// Herdr `plugin_context_for_tab_id`.
    fn ctx_for_tab(&self, server: &Server, t: &Tab, correlation: &str) -> Option<InvokeContext> {
        let w = self.ws(&t.workspace)?;
        Some(self.ctx_parts(server, w, Some(t), self.tab_focus(t), correlation))
    }
    /// Herdr `plugin_context_for_pane`: the pane is the focused pane.
    fn ctx_for_pane(&self, server: &Server, p: &Pane, correlation: &str) -> Option<InvokeContext> {
        let w = self.ws(&p.workspace)?;
        Some(self.ctx_parts(server, w, self.tab(&p.tab), Some(p), correlation))
    }
    /// Herdr `current_plugin_context`: the active workspace and its focused pane.
    fn current_ctx(&self, server: &Server, correlation: &str) -> InvokeContext {
        match self.active_ws() {
            Some(w) => self.ctx_for_ws(server, w, correlation),
            None => InvokeContext::empty(correlation),
        }
    }
    /// Herdr `plugin_context_for_event` for a projected event (`name` dotted, `d` its Herdr
    /// data); the correlation id is the event name, as in Herdr.
    fn ctx_for_event(&self, server: &Server, name: &str, d: &Value) -> InvokeContext {
        let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        let inner = |o: &str, k: &str| d.get(o).and_then(|v| s(v, k));
        let pane = s(d, "pane_id").or_else(|| inner("pane", "pane_id"));
        let tab = s(d, "tab_id")
            .or_else(|| inner("tab", "tab_id"))
            .or_else(|| inner("layout", "tab_id"));
        let ws = s(d, "workspace_id")
            .or_else(|| inner("workspace", "workspace_id"))
            .or_else(|| inner("tab", "workspace_id"))
            .or_else(|| inner("pane", "workspace_id"))
            .or_else(|| inner("layout", "workspace_id"))
            .or_else(|| {
                d.get("workspace_ids")
                    .and_then(|a| a.get(0))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
        let empty = || {
            let mut c = InvokeContext::empty(name);
            c.workspace_id = ws.clone();
            c.tab_id = tab.clone();
            c.focused_pane_id = pane.clone();
            if name == "worktree.removed" {
                c.workspace_label = inner("worktree", "label");
                c.workspace_cwd = inner("worktree", "path");
            }
            c
        };
        // Closed objects: only their ids (Herdr builds no context for them).
        if matches!(name, "pane.closed" | "tab.closed") {
            return empty();
        }
        pane.as_deref()
            .and_then(|p| self.pane(p))
            .and_then(|p| self.ctx_for_pane(server, p, name))
            .or_else(|| {
                tab.as_deref()
                    .and_then(|t| self.tab(t))
                    .and_then(|t| self.ctx_for_tab(server, t, name))
            })
            .or_else(|| {
                ws.as_deref()
                    .and_then(|w| self.ws(w))
                    .map(|w| self.ctx_for_ws(server, w, name))
            })
            .unwrap_or_else(empty)
    }
}

/// A reported `{key: "value"}` map with removed (`null`) keys left out; `None` when empty
/// (Herdr omits empty `tokens` and `state_labels`).
fn tokens(v: Option<&Value>) -> Option<Value> {
    let m: Map<String, Value> = v?
        .as_object()?
        .iter()
        .filter(|(_, x)| x.is_string())
        .map(|(k, x)| (k.clone(), x.clone()))
        .collect();
    (!m.is_empty()).then_some(Value::Object(m))
}

/// Herdr's `WorkspaceWorktreeInfo` for a workspace rooted at a git checkout, read from its
/// `.git` entry without running git: `repo_key` is the shared git directory, `repo_name` the
/// main checkout's directory name, `repo_root` the main checkout. `None` when the root is not
/// a checkout's top directory.
fn git_space(root: &str) -> Option<Value> {
    let checkout = Path::new(root);
    let dot = checkout.join(".git");
    let meta = std::fs::metadata(&dot).ok()?;
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let (common, linked) = if meta.is_dir() {
        (canon(&dot), false)
    } else {
        let text = std::fs::read_to_string(&dot).ok()?;
        let gitdir = checkout.join(text.lines().find_map(|l| l.strip_prefix("gitdir:"))?.trim());
        // A linked worktree's git dir names the shared one in `commondir`; a submodule's has none.
        match std::fs::read_to_string(gitdir.join("commondir")) {
            Ok(c) => (canon(&gitdir.join(c.trim())), true),
            Err(_) => (canon(&gitdir), false),
        }
    };
    let main = if common.file_name().is_some_and(|n| n == ".git") {
        common.parent().map(Path::to_path_buf)
    } else {
        None
    };
    let name = main
        .as_deref()
        .unwrap_or(common.as_path())
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("repo")
        .to_string();
    let repo_root = if linked {
        main.map(|m| m.display().to_string())
            .unwrap_or_else(|| root.to_string())
    } else {
        root.to_string()
    };
    Some(json!({
        "repo_key": common.display().to_string(),
        "repo_name": name,
        "repo_root": repo_root,
        "checkout_path": root,
        "is_linked_worktree": linked,
    }))
}

fn seeded_projector(server: &Server) -> Projector {
    let sn = snap(server);
    let mut p = Projector::new();
    let statuses: Vec<(String, String)> = sn
        .panes
        .iter()
        .filter_map(|x| {
            sn.pane_status(&x.id)
                .map(|s| (x.handle.clone(), s.to_string()))
        })
        .collect();
    let (ws, tab, _) = sn.focused();
    p.seed(tab.as_deref(), ws.as_deref(), statuses);
    p
}

/// Herdr events for one Vibeke event: `(dotted name, Herdr pane id, data)`. `data` holds
/// Herdr's per-event fields (schema `EventData`, without `type`); [`hev::envelope`] and
/// [`hev::stream_event`] wrap it.
fn project_event(
    server: &Server,
    proj: &mut Projector,
    ev: &vk_store::Event,
) -> Vec<(&'static str, Option<String>, Value)> {
    let subj = &ev.subject;
    let sid = |k: &str| subj.get(k).and_then(Value::as_str);
    // Remember the focus (Herdr has one even without a client attached).
    if ev.kind == "pane.focused"
        && let (Some(w), Some(p)) = (sid("workspace"), sid("pane"))
    {
        state(server)
            .focus_mem
            .lock()
            .unwrap()
            .record(w, sid("tab"), Some(p));
    }
    let sn = snap(server);
    // Popups have no pane lifecycle in the compat API (07 §7.7).
    if let Some(p) = sid("pane")
        && is_popup_pane(server, &sn.meta, p)
    {
        return vec![];
    }
    // Subject ids are ULIDs; translate to Herdr ids (handles), falling back to the handle the
    // subject carries or the one remembered for objects already gone.
    let known = |id: Option<&str>| id.and_then(|i| known_handle(server, i));
    let pane = sid("pane")
        .and_then(|p| sn.pane(p))
        .map(|p| p.handle.clone())
        .or_else(|| sid("pane_handle").map(str::to_string))
        .or_else(|| known(sid("pane")));
    let pane_obj = sid("pane").and_then(|p| sn.pane(p));
    let tab = sid("tab")
        .and_then(|t| sn.tab_handle(t))
        .or_else(|| pane_obj.and_then(|p| sn.tab_handle(&p.tab)))
        .or_else(|| known(sid("tab")));
    let ws = sid("workspace")
        .and_then(|w| sn.ws_handle(w))
        .or_else(|| pane_obj.and_then(|p| sn.ws_handle(&p.workspace)))
        .or_else(|| known(sid("workspace")))
        .or_else(|| {
            // A tab handle (`w1:t2`) names its workspace.
            tab.as_deref()
                .and_then(|t| t.split_once(':'))
                .map(|(w, _)| w.to_string())
        });
    let status_after = sid("pane").and_then(|p| sn.pane_status(p));
    // A block move (`workspace.move_block`) is one `workspace.moved` with the block in its data.
    let kind = if ev.kind == "workspace.moved" && ev.data.get("workspace_ids").is_some() {
        "workspace.reordered"
    } else {
        ev.kind.as_str()
    };
    let input = hev::Input {
        kind,
        workspace: ws.as_deref(),
        tab: tab.as_deref(),
        pane: pane.as_deref(),
        status_after,
        at_ms: ev.ts.max(0) as u64,
    };
    let names = proj.project(&input);
    let ids = Ids {
        ws: ws.clone(),
        tab: tab.clone(),
        pane: pane.clone(),
    };
    names
        .into_iter()
        .filter_map(|name| {
            event_data(server, &sn, ev, name, &ids, pane_obj).map(|d| (name, pane.clone(), d))
        })
        .collect()
}

/// The Herdr ids of a Vibeke event's subject.
struct Ids {
    ws: Option<String>,
    tab: Option<String>,
    pane: Option<String>,
}

/// Herdr's `EventData` fields of one projected event (schema `event.$defs.EventData`). `None`
/// when an object the event must carry is gone.
fn event_data(
    server: &Server,
    sn: &Snap,
    ev: &vk_store::Event,
    name: &str,
    ids: &Ids,
    pane_obj: Option<&Pane>,
) -> Option<Value> {
    let sid = |k: &str| ev.subject.get(k).and_then(Value::as_str);
    let ws_id = || json!(ids.ws.clone().unwrap_or_default());
    let tab_id = || json!(ids.tab.clone().unwrap_or_default());
    let pane_id = || json!(ids.pane.clone().unwrap_or_default());
    let ws_obj = || sid("workspace").and_then(|w| sn.ws(w));
    let tab_obj = || sid("tab").and_then(|t| sn.tab(t));
    let pane_json = || pane_obj.map(|p| sn.pane_json(server, p));
    let mut d = Map::new();
    match name {
        "workspace.created" | "workspace.updated" | "workspace.metadata_updated" => {
            d.insert("workspace".into(), sn.ws_json(ws_obj()?));
        }
        "workspace.closed" | "workspace.focused" => {
            d.insert("workspace_id".into(), ws_id());
        }
        "workspace.renamed" => {
            d.insert("workspace_id".into(), ws_id());
            d.insert(
                "label".into(),
                json!(ws_obj().map(|w| w.display_name().to_string())),
            );
        }
        "workspace.moved" => {
            let w = ws_obj()?;
            d.insert("workspace_id".into(), ws_id());
            d.insert(
                "insert_index".into(),
                json!(sn.ws.iter().position(|x| x.id == w.id).unwrap_or(0)),
            );
            d.insert(
                "workspaces".into(),
                json!(sn.ws.iter().map(|w| sn.ws_json(w)).collect::<Vec<_>>()),
            );
        }
        "workspace.reordered" => {
            // Herdr's `WorkspaceReordered {workspace_ids, before_workspace_id?, workspaces}`.
            let block = ev.data.get("workspace_ids").cloned().unwrap_or_else(|| {
                json!(sn.ws.iter().map(|w| w.handle.clone()).collect::<Vec<_>>())
            });
            d.insert("workspace_ids".into(), block);
            if let Some(b) = ev.data.get("before_workspace_id").filter(|b| b.is_string()) {
                d.insert("before_workspace_id".into(), b.clone());
            }
            d.insert(
                "workspaces".into(),
                json!(sn.ws.iter().map(|w| sn.ws_json(w)).collect::<Vec<_>>()),
            );
        }
        "worktree.created" | "worktree.opened" | "worktree.removed" => {
            let w = ws_obj().or_else(|| {
                let path = ev.data.get("path").and_then(Value::as_str)?;
                sn.ws.iter().find(|w| w.root_path == path)
            });
            let info = worktree_info(sn, ev, w);
            if name == "worktree.removed" {
                d.insert(
                    "workspace_id".into(),
                    json!(
                        w.map(|w| w.handle.clone())
                            .or(ids.ws.clone())
                            .or_else(|| ev.data["herdr_workspace"]["workspace_id"]
                                .as_str()
                                .map(str::to_string))
                            .unwrap_or_default()
                    ),
                );
                // `worktree.remove` closes the workspace first and carries its last snapshot.
                match (w, ev.data.get("herdr_workspace")) {
                    (Some(w), _) => {
                        d.insert("workspace".into(), sn.ws_json(w));
                    }
                    (None, Some(v)) if v.is_object() => {
                        d.insert("workspace".into(), v.clone());
                    }
                    _ => {}
                }
                d.insert("worktree".into(), info);
                d.insert(
                    "forced".into(),
                    json!(
                        ev.data
                            .get("forced")
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                    ),
                );
            } else {
                d.insert("workspace".into(), sn.ws_json(w?));
                d.insert("worktree".into(), info);
                if name == "worktree.opened" {
                    let created = ev
                        .data
                        .get("created_workspace")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    d.insert("already_open".into(), json!(!created));
                }
            }
        }
        "tab.created" => {
            d.insert("tab".into(), sn.tab_json(tab_obj()?));
        }
        "tab.closed" | "tab.focused" => {
            d.insert("tab_id".into(), tab_id());
            d.insert("workspace_id".into(), ws_id());
        }
        "tab.renamed" => {
            d.insert("tab_id".into(), tab_id());
            d.insert("workspace_id".into(), ws_id());
            d.insert("label".into(), json!(tab_obj().map(|t| sn.tab_label(t))));
        }
        "tab.moved" => {
            let t = tab_obj()?;
            d.insert("tab_id".into(), tab_id());
            d.insert("workspace_id".into(), ws_id());
            let tabs: Vec<&Tab> = sn
                .tabs
                .iter()
                .filter(|x| x.workspace == t.workspace)
                .collect();
            d.insert(
                "insert_index".into(),
                json!(tabs.iter().position(|x| x.id == t.id).unwrap_or(0)),
            );
            d.insert(
                "tabs".into(),
                json!(tabs.iter().map(|x| sn.tab_json(x)).collect::<Vec<_>>()),
            );
        }
        "pane.created" | "pane.updated" => {
            d.insert("pane".into(), pane_json()?);
        }
        "pane.closed" | "pane.focused" | "pane.exited" => {
            d.insert("pane_id".into(), pane_id());
            d.insert("workspace_id".into(), ws_id());
        }
        "pane.moved" => {
            // Vibeke keeps a pane's id when it moves: the previous id is the same.
            let from = ev.data.get("from_tab_id").and_then(Value::as_str);
            d.insert("previous_pane_id".into(), pane_id());
            d.insert(
                "previous_workspace_id".into(),
                json!(
                    from.and_then(|t| sn.tab(t))
                        .and_then(|t| sn.ws_handle(&t.workspace))
                        .or_else(|| from
                            .and_then(|t| t.split_once(':'))
                            .map(|(w, _)| w.to_string()))
                        .or(ids.ws.clone())
                        .unwrap_or_default()
                ),
            );
            d.insert(
                "previous_tab_id".into(),
                json!(
                    from.map(str::to_string)
                        .or(ids.tab.clone())
                        .unwrap_or_default()
                ),
            );
            d.insert("pane".into(), pane_json()?);
        }
        "pane.agent_detected" => {
            d.insert("pane_id".into(), pane_id());
            d.insert("workspace_id".into(), ws_id());
            if let Some(r) = pane_obj.and_then(|p| sn.pane_run.get(&p.id)) {
                d.insert("agent".into(), json!(status::herdr_agent_name(&r.harness)));
            }
        }
        "pane.agent_status_changed" => {
            let p = pane_json();
            d.insert("pane_id".into(), pane_id());
            d.insert("workspace_id".into(), ws_id());
            d.insert(
                "agent_status".into(),
                json!(
                    sid("pane")
                        .and_then(|x| sn.pane_status(x))
                        .unwrap_or("unknown")
                ),
            );
            for k in ["agent", "title", "display_agent", "state_labels"] {
                if let Some(v) = p.as_ref().and_then(|p| p.get(k)) {
                    d.insert(k.into(), v.clone());
                }
            }
        }
        "layout.updated" => {
            d.insert("layout".into(), ext::tab_snapshot(sn, tab_obj()?));
        }
        // Pane-scoped subscription events (Herdr `SubscriptionEventData`).
        "pane.scroll_changed" => {
            let mut scroll = pane_json()
                .and_then(|p| p.get("scroll").cloned())
                .unwrap_or_else(|| json!({"offset_from_bottom": 0, "max_offset_from_bottom": 0, "viewport_rows": 0}));
            if let Some(o) = ev.data.get("offset") {
                scroll["offset_from_bottom"] = o.clone();
            }
            d.insert("pane_id".into(), pane_id());
            d.insert("workspace_id".into(), ws_id());
            d.insert("scroll".into(), scroll);
        }
        "pane.output_matched" => {
            let line = match ev.data.get("matched") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Null) | None => String::new(),
                Some(v) => v.to_string(),
            };
            d.insert("pane_id".into(), pane_id());
            d.insert("matched_line".into(), json!(line));
            d.insert(
                "read".into(),
                json!({
                    "pane_id": pane_id(),
                    "workspace_id": ws_id(),
                    "tab_id": tab_id(),
                    "source": "recent",
                    "format": "text",
                    "text": line,
                    "revision": ev.data.get("revision").and_then(Value::as_u64).unwrap_or(0),
                    "truncated": false,
                }),
            );
        }
        _ => {}
    }
    Some(Value::Object(d))
}

/// Herdr's `WorktreeInfo` for a worktree event (`path`, `branch` from the Vibeke event).
fn worktree_info(sn: &Snap, ev: &vk_store::Event, w: Option<&Workspace>) -> Value {
    let path = ev
        .data
        .get("path")
        .or_else(|| ev.subject.get("path"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| w.map(|w| w.root_path.clone()))
        .unwrap_or_default();
    let branch = ev
        .data
        .get("branch")
        .and_then(Value::as_str)
        .map(str::to_string);
    let space = git_space(&path);
    let linked = space
        .as_ref()
        .and_then(|s| s["is_linked_worktree"].as_bool())
        .unwrap_or(true);
    let label = space
        .as_ref()
        .and_then(|s| s["repo_name"].as_str().map(str::to_string))
        .or_else(|| {
            ev.data
                .get("repo_root")
                .and_then(Value::as_str)
                .and_then(|r| Path::new(r).file_name())
                .and_then(|n| n.to_str())
                .map(str::to_string)
        })
        .unwrap_or_default();
    let open = sn
        .ws
        .iter()
        .find(|x| x.root_path == path)
        .map(|x| x.handle.clone());
    let mut v = Map::new();
    v.insert("path".into(), json!(path));
    if let Some(b) = &branch {
        v.insert("branch".into(), json!(b));
    }
    v.insert("is_bare".into(), json!(false));
    v.insert("is_detached".into(), json!(branch.is_none()));
    v.insert("is_prunable".into(), json!(false));
    v.insert("is_linked_worktree".into(), json!(linked));
    if let Some(o) = open {
        v.insert("open_workspace_id".into(), json!(o));
    }
    v.insert("label".into(), json!(label));
    Value::Object(v)
}

// ---- method mapping ---------------------------------------------------------------------------

fn native_err(e: vk_proto::rpc::RpcError) -> WireError {
    let object = e.data.details.get("object").and_then(Value::as_str);
    wire::from_native(&e.data.kind, object, &e.message)
}

async fn native(
    server: &Arc<Server>,
    caller: &Caller,
    method: &str,
    p: Value,
) -> Result<Value, WireError> {
    // Boxed: `api::dispatch` reaches back into this module (`compat.herdr.call`).
    Box::pin(api::dispatch(server, &caller.ctx, method, &p))
        .await
        .map_err(native_err)
}

fn sp<'a>(p: &'a Value, k: &str) -> Option<&'a str> {
    p.get(k).and_then(Value::as_str)
}

fn req_s<'a>(p: &'a Value, k: &str) -> Result<&'a str, WireError> {
    sp(p, k).ok_or_else(|| WireError::new("invalid_params", format!("{k} is required")))
}

/// Herdr's `sanitized_notification_text`: line breaks and tabs become spaces, other control
/// characters go, runs of whitespace collapse, at most `max` characters, trimmed; `None` when
/// nothing is left.
fn sanitized_notification(value: &str, max: usize) -> Option<String> {
    let mut out = String::new();
    let mut count = 0usize;
    let mut space = false;
    for ch in value.chars() {
        let ch = match ch {
            '\n' | '\r' | '\t' => ' ',
            c if c.is_control() => continue,
            c => c,
        };
        if ch.is_whitespace() {
            if space {
                continue;
            }
            space = true;
            out.push(' ');
        } else {
            space = false;
            out.push(ch);
        }
        count += 1;
        if count >= max {
            break;
        }
    }
    let t = out.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// Herdr's `normalize_launch_env` for the `env` of `pane.split`, `tab.create` and
/// `workspace.create` (`invalid_env`).
fn herdr_launch_env(p: &Value) -> Result<Vec<(String, String)>, WireError> {
    launch::launch_env(p.get("env")).map_err(|(code, msg)| WireError::new(code, msg))
}

fn env_json(env: &[(String, String)]) -> Value {
    Value::Object(
        env.iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect(),
    )
}

/// A native create error as Herdr's `<thing>_create_failed` (`pane_split_failed`), keeping
/// Herdr's not-found and permission codes.
fn create_failed(code: &str, e: WireError) -> WireError {
    match e.code.as_str() {
        "pane_not_found"
        | "workspace_not_found"
        | "tab_not_found"
        | "permission_denied"
        | "invalid_params"
        | "invalid_env" => e,
        _ => WireError::new(code, e.message),
    }
}

/// The directory a new pane follows in a workspace: its focused pane's current directory,
/// else the workspace root.
fn ws_cwd(server: &Server, sn: &Snap, w: &Workspace) -> String {
    sn.active_tab(w)
        .and_then(|t| sn.tab_focus(t))
        .and_then(|x| server.pane_cwd(&x.id))
        .unwrap_or_else(|| w.root_path.clone())
}

/// The pane a request targets: `pane_id`; else, for a pane-scoped caller, its own pane;
/// else Herdr's focused pane (`resolve_optional_pane`): the most recent client's focus, else
/// the pane the invocation's context named, else the active workspace's focused pane.
fn pane_target(caller: &Caller, sn: &Snap, p: &Value) -> Result<String, WireError> {
    if let Some(id) = sp(p, "pane_id") {
        return sn
            .pane(id)
            .map(|x| x.id.clone())
            .ok_or_else(|| WireError::new("pane_not_found", format!("pane {id} not found")));
    }
    if let Some(s) = &caller.ctx.pane_scope {
        return Ok(s.clone());
    }
    if let Some(x) = sn.focus.pane.as_deref().and_then(|f| sn.pane(f)) {
        return Ok(x.id.clone());
    }
    if let Some(x) = caller.default_pane.as_deref().and_then(|d| sn.pane(d)) {
        return Ok(x.id.clone());
    }
    focused_pane(sn).ok_or_else(|| WireError::new("pane_not_found", "pane not found"))
}

/// The active workspace's focused pane (Herdr's `resolve_optional_pane(None)`).
fn focused_pane(sn: &Snap) -> Option<String> {
    sn.active_ws()
        .and_then(|w| sn.active_tab(w))
        .and_then(|t| sn.tab_focus(t))
        .map(|x| x.id.clone())
}

/// The pane a Herdr `PaneTarget` names: `pane_id` is required (no focus fallback).
fn herdr_pane(sn: &Snap, p: &Value) -> Result<Pane, WireError> {
    let id = req_s(p, "pane_id")?;
    sn.pane(id)
        .cloned()
        .ok_or_else(|| WireError::new("pane_not_found", format!("pane {id} not found")))
}

fn ws_target(sn: &Snap, p: &Value) -> Result<Workspace, WireError> {
    let id = req_s(p, "workspace_id")?;
    sn.ws(id)
        .cloned()
        .ok_or_else(|| WireError::new("workspace_not_found", format!("workspace {id} not found")))
}

fn tab_target(sn: &Snap, p: &Value) -> Result<Tab, WireError> {
    let id = req_s(p, "tab_id")?;
    sn.tab(id)
        .cloned()
        .ok_or_else(|| WireError::new("tab_not_found", format!("tab {id} not found")))
}

fn run_target(sn: &Snap, p: &Value) -> Result<AgentRun, WireError> {
    let t = req_s(p, "target").or_else(|_| req_s(p, "agent_id"))?;
    sn.pane_run
        .values()
        .find(|r| r.handle == t || r.id == t || r.name.as_deref() == Some(t))
        .or_else(|| sn.pane(t).and_then(|x| sn.pane_run.get(&x.id)))
        .cloned()
        .ok_or_else(|| WireError::new("agent_not_found", format!("agent target {t} not found")))
}

fn ok() -> Value {
    typed("ok", json!({}))
}

/// Dispatch one Herdr request. `events.subscribe` streams and is handled by [`serve_wire`].
pub async fn call(
    server: &Arc<Server>,
    caller: &Caller,
    method: &str,
    p: &Value,
) -> Result<Value, WireError> {
    // Focus-changing operations act on the user's most recently active client, like Herdr's
    // single focus.
    let mut caller = caller.clone();
    if caller.ctx.pane_scope.is_none()
        && let Some(c) = crate::notify::recent_client(server)
    {
        caller.ctx.client_id = c;
    }
    let caller = &caller;
    if caller.sandboxed && !sandbox_allows(method) {
        return Err(sandbox_denied(method));
    }
    let sn = snap(server);
    match method {
        // Herdr `pong {version, protocol, capabilities}`; Vibeke adds who answers.
        "ping" => Ok(typed(
            "pong",
            json!({
                "version": herdr::BASELINE_VERSION,
                "protocol": herdr::cli::PROTOCOL_VERSION,
                "capabilities": {
                    "live_handoff": false,
                    "detached_server_daemon": false,
                    "ssh_agent_registration": false,
                    "surface_interest": false,
                    "health_check": false,
                    "endpoint_protocol_generation": herdr::cli::ENDPOINT_PROTOCOL_GENERATION,
                },
                "emulator": "vibeke",
                "vibeke_version": vk_proto::VERSION,
                "compatibility": "partial",
            }),
        )),
        "api.schema" => Ok(typed(
            "api_schema",
            json!({
                "version": herdr::BASELINE_VERSION,
                "methods": inventory::ENTRIES.iter().filter(|e| e.kind == inventory::Kind::Method).map(|e| json!({"name": e.name, "status": e.status.as_str()})).collect::<Vec<_>>(),
                "events": hev::BASELINE_EVENTS,
            }),
        )),
        "session.snapshot" => Ok(typed(
            "session_snapshot",
            json!({"snapshot": session_snapshot(server, &sn)}),
        )),
        // ---- workspaces ------------------------------------------------------------------
        "workspace.list" => Ok(typed(
            "workspace_list",
            json!({"workspaces": sn.ws.iter().map(|w| sn.ws_json(w)).collect::<Vec<_>>()}),
        )),
        "workspace.get" => {
            let w = ws_target(&sn, p)?;
            Ok(typed(
                "workspace_info",
                json!({"workspace": sn.ws_json(&w)}),
            ))
        }
        "workspace.create" => {
            let env = herdr_launch_env(p)?;
            let mut np = json!({"focus": p.get("focus").and_then(Value::as_bool).unwrap_or(false)});
            // Herdr: `cwd`, else the focused pane's directory in `source_workspace_id` (else
            // the active workspace).
            let cwd = match sp(p, "cwd") {
                Some(c) => Some(c.to_string()),
                None => {
                    let src = match sp(p, "source_workspace_id") {
                        Some(id) => Some(sn.ws(id).ok_or_else(|| {
                            WireError::new(
                                "workspace_not_found",
                                format!("workspace {id} not found"),
                            )
                        })?),
                        None => sn.active_ws(),
                    };
                    src.map(|w| ws_cwd(server, &sn, w))
                }
            };
            if let Some(c) = cwd {
                np["cwd"] = json!(c);
            }
            if let Some(l) = sp(p, "label") {
                np["name"] = json!(l);
            }
            if !env.is_empty() {
                np["env"] = env_json(&env);
            }
            let r = native(server, caller, "workspace.create", np)
                .await
                .map_err(|e| create_failed("workspace_create_failed", e))?;
            let sn = snap(server);
            let id = |k: &str| r[k]["id"].as_str().unwrap_or_default().to_string();
            Ok(typed(
                "workspace_created",
                json!({
                    "workspace": sn.ws(&id("workspace")).map(|w| sn.ws_json(w)),
                    "tab": sn.tab(&id("tab")).map(|t| sn.tab_json(t)),
                    "root_pane": sn.pane(&id("root_pane")).map(|x| sn.pane_json(server, x)),
                }),
            ))
        }
        "workspace.rename" => {
            let w = ws_target(&sn, p)?;
            // Herdr stores an empty label literally; Vibeke's empty name falls back to the
            // automatic name (documented difference).
            let label = sp(p, "label").unwrap_or("");
            native(
                server,
                caller,
                "workspace.rename",
                json!({"workspace": w.id, "name": label}),
            )
            .await?;
            let sn = snap(server);
            Ok(typed(
                "workspace_info",
                json!({"workspace": sn.ws(&w.id).map(|w| sn.ws_json(w))}),
            ))
        }
        "workspace.move" => {
            let w = ws_target(&sn, p)?;
            let to = p
                .get("insert_index")
                .and_then(Value::as_i64)
                .ok_or_else(|| WireError::new("invalid_params", "insert_index is required"))?;
            let from = sn.ws.iter().position(|x| x.id == w.id).unwrap_or(0) as i64;
            native(
                server,
                caller,
                "workspace.move",
                json!({"workspace": w.id, "delta": to - from}),
            )
            .await?;
            let sn = snap(server);
            Ok(typed(
                "workspace_list",
                json!({"workspaces": sn.ws.iter().map(|w| sn.ws_json(w)).collect::<Vec<_>>()}),
            ))
        }
        "workspace.focus" => {
            let w = ws_target(&sn, p)?;
            native(
                server,
                caller,
                "workspace.focus",
                json!({"workspace": w.id}),
            )
            .await?;
            let sn = snap(server);
            Ok(typed(
                "workspace_info",
                json!({"workspace": sn.ws(&w.id).map(|w| sn.ws_json(w))}),
            ))
        }
        "workspace.close" => {
            let w = ws_target(&sn, p)?;
            native(
                server,
                caller,
                "workspace.close",
                json!({"workspace": w.id}),
            )
            .await?;
            Ok(ok())
        }
        "workspace.move_block" => workspace_move_block(server, caller, &sn, p).await,
        // ---- tabs ------------------------------------------------------------------------
        "tab.list" => {
            let ws = match sp(p, "workspace_id") {
                Some(_) => Some(ws_target(&sn, p)?.id),
                None => None,
            };
            Ok(typed(
                "tab_list",
                json!({"tabs": sn.tabs.iter().filter(|t| ws.as_ref().is_none_or(|w| &t.workspace == w)).map(|t| sn.tab_json(t)).collect::<Vec<_>>()}),
            ))
        }
        "tab.create" => {
            let w = match sp(p, "workspace_id") {
                Some(_) => ws_target(&sn, p)?,
                None => sn
                    .focus
                    .workspace
                    .as_deref()
                    .and_then(|w| sn.ws(w))
                    .or(sn.ws.first())
                    .cloned()
                    .ok_or_else(|| WireError::new("workspace_not_found", "no workspace"))?,
            };
            let env = herdr_launch_env(p)?;
            let mut np = json!({"workspace": w.id, "focus": p.get("focus").and_then(Value::as_bool).unwrap_or(false)});
            if let Some(l) = sp(p, "label") {
                np["title"] = json!(l);
            }
            // Herdr: `cwd`, else the directory of the workspace's focused pane.
            np["cwd"] = json!(
                sp(p, "cwd")
                    .map(str::to_string)
                    .unwrap_or_else(|| ws_cwd(server, &sn, &w))
            );
            if !env.is_empty() {
                np["env"] = env_json(&env);
            }
            let r = native(server, caller, "tab.create", np)
                .await
                .map_err(|e| create_failed("tab_create_failed", e))?;
            let sn = snap(server);
            let id = |k: &str| r[k]["id"].as_str().unwrap_or_default().to_string();
            Ok(typed(
                "tab_created",
                json!({
                    "tab": sn.tab(&id("tab")).map(|t| sn.tab_json(t)),
                    "root_pane": sn.pane(&id("root_pane")).map(|x| sn.pane_json(server, x)),
                }),
            ))
        }
        "tab.rename" => {
            let t = tab_target(&sn, p)?;
            native(
                server,
                caller,
                "tab.rename",
                json!({"tab": t.id, "title": sp(p, "label").unwrap_or("")}),
            )
            .await?;
            let sn = snap(server);
            Ok(typed(
                "tab_info",
                json!({"tab": sn.tab(&t.id).map(|t| sn.tab_json(t))}),
            ))
        }
        "tab.move" => {
            let t = tab_target(&sn, p)?;
            let to = p
                .get("insert_index")
                .and_then(Value::as_i64)
                .ok_or_else(|| WireError::new("invalid_params", "insert_index is required"))?;
            let siblings: Vec<&Tab> = sn
                .tabs
                .iter()
                .filter(|x| x.workspace == t.workspace)
                .collect();
            let from = siblings.iter().position(|x| x.id == t.id).unwrap_or(0) as i64;
            native(
                server,
                caller,
                "tab.move",
                json!({"tab": t.id, "delta": to - from}),
            )
            .await?;
            let sn = snap(server);
            Ok(typed(
                "tab_list",
                json!({"tabs": sn.tabs.iter().filter(|x| x.workspace == t.workspace).map(|x| sn.tab_json(x)).collect::<Vec<_>>()}),
            ))
        }
        "tab.focus" => {
            let t = tab_target(&sn, p)?;
            native(server, caller, "tab.focus", json!({"tab": t.id})).await?;
            let sn = snap(server);
            Ok(typed(
                "tab_info",
                json!({"tab": sn.tab(&t.id).map(|t| sn.tab_json(t))}),
            ))
        }
        "tab.close" => {
            let t = tab_target(&sn, p)?;
            native(server, caller, "tab.close", json!({"tab": t.id})).await?;
            Ok(ok())
        }
        "tab.get" => {
            let t = tab_target(&sn, p)?;
            Ok(typed("tab_info", json!({"tab": sn.tab_json(&t)})))
        }
        // ---- panes -----------------------------------------------------------------------
        "pane.list" => {
            let ws = match sp(p, "workspace_id") {
                Some(_) => Some(ws_target(&sn, p)?.id),
                None => None,
            };
            let tab = match sp(p, "tab_id") {
                Some(_) => Some(tab_target(&sn, p)?.id),
                None => None,
            };
            let panes: Vec<Value> = sn
                .panes
                .iter()
                .filter(|x| ws.as_ref().is_none_or(|w| &x.workspace == w))
                .filter(|x| tab.as_ref().is_none_or(|t| &x.tab == t))
                .filter(|x| !x.exited)
                .map(|x| sn.pane_json(server, x))
                .collect();
            Ok(typed("pane_list", json!({"panes": panes})))
        }
        "pane.get" => {
            let id = pane_target(caller, &sn, p)?;
            let x = sn
                .pane(&id)
                .ok_or_else(|| WireError::new("pane_not_found", format!("pane {id} not found")))?;
            Ok(typed("pane_info", json!({"pane": sn.pane_json(server, x)})))
        }
        "pane.current" => {
            // Herdr: `caller_pane_id` (the CLI sends `HERDR_PANE_ID`), else the focused pane;
            // `pane_current {pane}`.
            let id = match sp(p, "caller_pane_id") {
                Some(c) => sn.pane(c).map(|x| x.id.clone()),
                None => pane_target(caller, &sn, &json!({})).ok(),
            }
            .ok_or_else(|| WireError::new("pane_not_found", "pane not found"))?;
            let x = sn
                .pane(&id)
                .ok_or_else(|| WireError::new("pane_not_found", "pane not found"))?;
            Ok(typed(
                "pane_current",
                json!({"pane": sn.pane_json(server, x)}),
            ))
        }
        "pane.read" => {
            let id = pane_target(caller, &sn, p)?;
            let x = sn
                .pane(&id)
                .cloned()
                .ok_or_else(|| WireError::new("pane_not_found", format!("pane {id} not found")))?;
            let read = herdr_read(server, caller, &sn, &x, p).await?;
            Ok(typed("pane_read", json!({"read": read})))
        }
        "pane.send_text" => {
            let id = pane_target(caller, &sn, p)?;
            let text = req_s(p, "text")?;
            native(
                server,
                caller,
                "pane.send_text",
                json!({"pane": id, "text": text, "paste": "raw"}),
            )
            .await?;
            Ok(ok())
        }
        "pane.send_keys" => {
            let id = pane_target(caller, &sn, p)?;
            native(
                server,
                caller,
                "pane.send_keys",
                json!({"pane": id, "keys": keys(p)?}),
            )
            .await?;
            Ok(ok())
        }
        "pane.send_input" => {
            let id = pane_target(caller, &sn, p)?;
            if let Some(text) = sp(p, "text") {
                native(
                    server,
                    caller,
                    "pane.send_text",
                    json!({"pane": id, "text": text, "paste": "raw"}),
                )
                .await?;
            }
            if p.get("keys").is_some() {
                native(
                    server,
                    caller,
                    "pane.send_keys",
                    json!({"pane": id, "keys": keys(p)?}),
                )
                .await?;
            }
            Ok(ok())
        }
        "pane.run" => {
            let id = pane_target(caller, &sn, p)?;
            native(
                server,
                caller,
                "pane.run",
                json!({"pane": id, "command": req_s(p, "command")?}),
            )
            .await?;
            Ok(ok())
        }
        "pane.focus" => {
            let id = pane_target(caller, &sn, p)?;
            native(server, caller, "pane.focus", json!({"pane": id})).await?;
            let sn = snap(server);
            let x = sn
                .pane(&id)
                .ok_or_else(|| WireError::new("pane_not_found", format!("pane {id} not found")))?;
            Ok(typed("pane_info", json!({"pane": sn.pane_json(server, x)})))
        }
        "pane.rename" => {
            let id = pane_target(caller, &sn, p)?;
            let label = sp(p, "label").unwrap_or("");
            native(
                server,
                caller,
                "pane.rename",
                json!({"pane": id, "title": label}),
            )
            .await?;
            let sn = snap(server);
            let x = sn
                .pane(&id)
                .ok_or_else(|| WireError::new("pane_not_found", format!("pane {id} not found")))?;
            Ok(typed("pane_info", json!({"pane": sn.pane_json(server, x)})))
        }
        "pane.close" => {
            let id = pane_target(caller, &sn, p)?;
            native(server, caller, "pane.close", json!({"pane": id})).await?;
            Ok(ok())
        }
        "pane.split" => {
            // Herdr: `target_pane_id`, else the focused pane of `workspace_id`, else of the
            // active workspace (Vibeke also takes `pane_id` and the caller's pane).
            let nf = || WireError::new("pane_not_found", "pane not found");
            let id = if let Some(t) = sp(p, "target_pane_id") {
                sn.pane(t).map(|x| x.id.clone()).ok_or_else(nf)?
            } else if let Some(w) = sp(p, "workspace_id") {
                let w = sn.ws(w).ok_or_else(nf)?;
                sn.active_tab(w)
                    .and_then(|t| sn.tab_focus(t))
                    .map(|x| x.id.clone())
                    .ok_or_else(nf)?
            } else {
                pane_target(caller, &sn, p)?
            };
            match p.get("right_click") {
                None | Some(Value::Null) => {}
                Some(Value::String(r)) if r == "herdr" || r == "pane" => {}
                Some(other) => {
                    return Err(wire::invalid_request(format!(
                        "unknown variant {other}, expected `herdr` or `pane`"
                    )));
                }
            }
            let env = herdr_launch_env(p)?;
            let mut np = json!({"pane": id, "direction": sp(p, "direction").unwrap_or("right"), "focus": p.get("focus").and_then(Value::as_bool).unwrap_or(false)});
            if let Some(c) = sp(p, "cwd") {
                np["cwd"] = json!(c);
            }
            if !env.is_empty() {
                np["env"] = env_json(&env);
            }
            // Herdr's `ratio` is the share of the split pane (the first side), clamped to
            // 0.1..0.9; Vibeke's is the new pane's share.
            if let Some(r) = p.get("ratio").and_then(Value::as_f64) {
                let r = if r.is_finite() {
                    r.clamp(0.1, 0.9)
                } else {
                    0.5
                };
                np["ratio"] = json!(1.0 - r);
            }
            let r = native(server, caller, "pane.split", np)
                .await
                .map_err(|e| create_failed("pane_split_failed", e))?;
            let sn = snap(server);
            let new = r["pane"]["id"].as_str().unwrap_or_default();
            Ok(typed(
                "pane_info",
                json!({"pane": sn.pane(new).map(|x| sn.pane_json(server, x))}),
            ))
        }
        "pane.wait_for_output" => {
            let id = pane_target(caller, &sn, p)?;
            let x = sn
                .pane(&id)
                .cloned()
                .ok_or_else(|| WireError::new("pane_not_found", format!("pane {id} not found")))?;
            wait_for_output(server, caller, &sn, &x, p).await
        }
        "pane.report_agent" | "pane.report_agent_session" => {
            let mut np = p.clone();
            if let Some(id) = sp(p, "pane_id") {
                let x = sn.pane(id).ok_or_else(|| {
                    WireError::new("pane_not_found", format!("pane {id} not found"))
                })?;
                np["pane_id"] = json!(x.id);
            } else if let Some(d) = &caller.default_pane {
                np["pane_id"] = json!(d);
            }
            let r = native(server, caller, method, np.clone()).await?;
            // The reporting hook holds the pane's authority (Herdr's `hook_authority`).
            if method == "pane.report_agent"
                && r.get("dropped").is_none()
                && let (Some(pane), Some(agent)) = (sp(&np, "pane_id"), sp(p, "agent"))
                && let Some(x) = snap(server).pane(pane)
            {
                let source = sp(p, "source").unwrap_or("herdr").to_string();
                state(server)
                    .authority
                    .lock()
                    .unwrap()
                    .insert(x.id.clone(), (source, agent.trim().to_string()));
            }
            Ok(ok())
        }
        "pane.resize" => pane_resize(server, caller, &sn, p).await,
        "pane.zoom" => pane_zoom(server, caller, &sn, p).await,
        "pane.input.set" => {
            // Herdr routes right clicks to the pane's program (`pane`) or to its own menu
            // (`herdr`). Vibeke's clients have no per-pane right-click routing: the setting is
            // validated and acknowledged only.
            herdr_pane(&sn, p)?;
            match sp(p, "right_click") {
                Some("herdr" | "pane") => {}
                Some(o) => {
                    return Err(WireError::new(
                        "invalid_params",
                        format!("unknown variant `{o}`, expected `herdr` or `pane`"),
                    ));
                }
                None => return Err(WireError::new("invalid_params", "right_click is required")),
            }
            Ok(ok())
        }
        "pane.release_agent" | "pane.clear_agent_authority" => {
            let x = herdr_pane(&sn, p)?;
            if let Some(scope) = &caller.ctx.pane_scope
                && &x.id != scope
                && x.created_by != format!("agent:{scope}")
            {
                return Err(WireError::new(
                    "permission_denied",
                    "self-report is limited to your own pane",
                ));
            }
            // Herdr's per-source hook authority: the last accepted report's `(source, agent)`,
            // kept while that self-reported run lives.
            let run = sn
                .pane_run
                .get(&x.id)
                .filter(|r| r.ended_at_ms.is_none() && r.integration == "self_report")
                .cloned();
            let st = state(server);
            let auth = {
                let mut a = st.authority.lock().unwrap();
                if run.is_none() {
                    a.remove(&x.id);
                }
                a.get(&x.id).cloned()
            };
            let seq = p
                .get("seq")
                .and_then(Value::as_u64)
                .map(|s| i64::try_from(s).unwrap_or(i64::MAX));
            let accept = |source: &str| crate::agents::selfreport::accept_seq(&x.id, source, seq);
            let act = if method == "pane.release_agent" {
                let source = req_s(p, "source")?;
                let agent = req_s(p, "agent")?.trim();
                if agent.is_empty() {
                    return Err(WireError::new(
                        "invalid_agent",
                        "agent label must not be empty",
                    ));
                }
                // Herdr ignores releases from its own integrations and from any source or
                // agent other than the one holding the authority; the agent must be the
                // pane's current one; a stale `seq` is dropped.
                !status::official_agent_source(source, agent)
                    && auth.as_ref().is_none_or(|(s, a)| s == source && a == agent)
                    && run.as_ref().is_some_and(|r| {
                        auth.is_some() || r.harness == status::vibeke_harness(agent)
                    })
                    && accept(source)
            } else {
                // `clear_agent_authority`: only the holder's source (or any, without one).
                match &auth {
                    Some((holder, _)) => {
                        let source = sp(p, "source");
                        source.is_none_or(|s| s == holder) && accept(source.unwrap_or(holder))
                    }
                    None => false,
                }
            };
            // Detection takes over again: a run that only self-reports ends.
            if act {
                st.authority.lock().unwrap().remove(&x.id);
                if let Some(r) = &run {
                    server.agents.end_run(server, &r.id, "released");
                }
            }
            Ok(ok())
        }
        "pane.scroll" => {
            let x = herdr_pane(&sn, p)?;
            let want = p
                .get("offset_from_bottom")
                .and_then(Value::as_u64)
                .ok_or_else(|| {
                    WireError::new("invalid_params", "offset_from_bottom is required")
                })?;
            let now = native(
                server,
                caller,
                "pane.scroll",
                json!({"pane": x.id, "delta": 0}),
            )
            .await?;
            let cur = now["scroll"]["offset"].as_u64().unwrap_or(0);
            let total = now["scroll"]["total"].as_u64().unwrap_or(0);
            let to = want.min(total);
            if to != cur {
                native(
                    server,
                    caller,
                    "pane.scroll",
                    json!({"pane": x.id, "delta": to as i64 - cur as i64}),
                )
                .await?;
            }
            scroll_changed(
                server,
                "api",
                &x.id,
                u32::try_from(to).unwrap_or(u32::MAX),
                u32::try_from(total).unwrap_or(u32::MAX),
            );
            let sn = snap(server);
            let x = sn.pane(&x.id).ok_or_else(|| {
                WireError::new("pane_not_found", format!("pane {} not found", x.handle))
            })?;
            Ok(typed("pane_info", json!({"pane": sn.pane_json(server, x)})))
        }
        // Herdr behaviours that need its own client or state; answered as Herdr answers when
        // that state is absent, or `unsupported` with the reason.
        "server.ssh_agent.register" => Err(WireError::new(
            "connection_local_only",
            "SSH agent registration requires a persistent local JSON API connection",
        )),
        "release_notes.dismiss" => {
            req_s(p, "version")?;
            Err(WireError::new(
                "stale_release_notes",
                "the release notes are no longer current",
            ))
        }
        "product_announcement.dismiss" => {
            req_s(p, "version")?;
            req_s(p, "id")?;
            Err(WireError::new(
                "stale_announcement",
                "the product announcement is no longer current",
            ))
        }
        // ---- agents ----------------------------------------------------------------------
        "agent.list" => Ok(typed(
            "agent_list",
            json!({"agents": sn.pane_run.values().map(|r| sn.agent_json(server, r)).collect::<Vec<_>>()}),
        )),
        "agent.get" => {
            let r = run_target(&sn, p)?;
            Ok(typed(
                "agent_info",
                json!({"agent": sn.agent_json(server, &r)}),
            ))
        }
        "agent.send" => {
            let r = run_target(&sn, p)?;
            native(
                server,
                caller,
                "pane.send_text",
                json!({"pane": r.pane, "text": req_s(p, "text")?, "paste": "raw"}),
            )
            .await?;
            Ok(ok())
        }
        "agent.read" => {
            let r = run_target(&sn, p)?;
            let x = sn
                .pane(&r.pane)
                .cloned()
                .ok_or_else(|| WireError::new("agent_not_found", "agent pane not found"))?;
            let read = herdr_read(server, caller, &sn, &x, p).await?;
            Ok(typed("pane_read", json!({"read": read})))
        }
        "agent.focus" => {
            // Herdr: focus the agent's pane (and its tab and workspace), answer `agent_info`.
            let r = run_target(&sn, p)?;
            native(server, caller, "pane.focus", json!({"pane": r.pane})).await?;
            let sn = snap(server);
            let r = sn.pane_run.get(&r.pane).cloned().unwrap_or(r);
            Ok(typed(
                "agent_info",
                json!({"agent": sn.agent_json(server, &r)}),
            ))
        }
        "agent.send_keys" => {
            let r = run_target(&sn, p)?;
            if r.ended_at_ms.is_some() {
                return Err(WireError::new(
                    "agent_not_ready",
                    format!("agent {} is not an active named agent", r.handle),
                ));
            }
            let k = match p.get("keys") {
                Some(Value::Array(a)) => Value::Array(a.clone()),
                _ => return Err(WireError::new("invalid_params", "keys is required")),
            };
            native(
                server,
                caller,
                "pane.send_keys",
                json!({"pane": r.pane, "keys": k}),
            )
            .await?;
            Ok(ok())
        }
        // ---- worktrees, notifications, server --------------------------------------------
        "worktree.list" => worktree_list(server, caller, &sn, p).await,
        "pane.link.resolve" | "pane.link.activate" => {
            pane_link(server, caller, &sn, method, p).await
        }
        "worktree.repo_root" => {
            let r = native(
                server,
                caller,
                "worktree.repo_root",
                json!({"cwd": req_s(p, "cwd")?}),
            )
            .await?;
            Ok(typed(
                "worktree_repo_root",
                json!({"repo_root": r["repo_root"]}),
            ))
        }
        "notification.show" => {
            let raw = req_s(p, "title")?;
            let title = sanitized_notification(raw, 80)
                .ok_or_else(|| WireError::new("invalid_params", "notification title is empty"))?;
            let body = sp(p, "body")
                .or(sp(p, "message"))
                .and_then(|b| sanitized_notification(b, 240));
            // Herdr shows at most one API notification per second (`rate_limited`).
            let now = std::time::Instant::now();
            {
                let st = state(server);
                let mut last = st.last_notification.lock().unwrap();
                if last.is_some_and(|t| now.duration_since(t) < Duration::from_secs(1)) {
                    return Ok(typed(
                        "notification_show",
                        json!({"shown": false, "reason": "rate_limited"}),
                    ));
                }
                *last = Some(now);
            }
            let mut np = json!({"title": title});
            if let Some(b) = body {
                np["body"] = json!(b);
            }
            native(server, caller, "notification.send", np).await?;
            // Herdr `notification_show {shown, reason}`; Vibeke delivers through its own
            // notifier, so a sent notification counts as shown.
            Ok(typed(
                "notification_show",
                json!({"shown": true, "reason": "shown"}),
            ))
        }
        // Herdr reloads its config.toml; Vibeke reloads its own (`server.reload_config`) and
        // answers in Herdr's shape: `failed` when the file does not load, `partial` with
        // diagnostics, else `applied`.
        "server.reload_config" => {
            let r = native(server, caller, "server.reload_config", json!({})).await?;
            let list = |k: &str| r[k].as_array().cloned().unwrap_or_default();
            let (errors, warnings) = (list("errors"), list("warnings"));
            let line = |d: &Value| {
                let msg = d["message"].as_str().unwrap_or("");
                match (d["key"].as_str(), d["line"].as_u64(), d["col"].as_u64()) {
                    (Some(k), _, _) => format!("{k}: {msg}"),
                    (None, Some(l), Some(c)) => format!("line {l}, column {c}: {msg}"),
                    _ => msg.to_string(),
                }
            };
            let status = if !errors.is_empty() {
                "failed"
            } else if !warnings.is_empty() {
                "partial"
            } else {
                "applied"
            };
            let diagnostics: Vec<String> = errors.iter().chain(&warnings).map(line).collect();
            Ok(typed(
                "config_reload",
                json!({"status": status, "diagnostics": diagnostics}),
            ))
        }
        "events.wait" => events_wait(server, caller, p).await,
        "vibeke.invocation_ticket" => invocation_ticket(server, caller),
        "events.subscribe" => Err(WireError::new(
            "invalid_request",
            "events.subscribe streams on its own socket connection",
        )),
        // ---- plugins ---------------------------------------------------------------------
        "plugin.list" => Ok(typed(
            "plugin_list",
            json!({"plugins": herdr_plugin_list(sp(p, "plugin_id"))}),
        )),
        "plugin.action.list" => Ok(typed(
            "plugin_action_list",
            json!({"actions": herdr_action_list(sp(p, "plugin_id"))?}),
        )),
        "plugin.action.invoke" => {
            // Baseline: `action_id` is the qualified `<plugin>.<action>` id (as listed in
            // `qualified_id`); `plugin_id` is optional. Vibeke also accepts `plugin_id` with an
            // unqualified `action_id`, and the older `action`.
            let (plugin, action) = match (sp(p, "plugin_id"), sp(p, "action_id"), sp(p, "action")) {
                (Some(pl), Some(a), _) => (
                    pl.to_string(),
                    a.strip_prefix(&format!("{pl}.")).unwrap_or(a).to_string(),
                ),
                (None, Some(q), _) | (None, None, Some(q)) => split_qualified(q)?,
                (Some(pl), None, Some(a)) => (pl.to_string(), a.to_string()),
                _ => {
                    return Err(WireError::new("invalid_params", "action_id is required"));
                }
            };
            if caller.sandboxed {
                // Only its own actions, and only while they would run restricted as well.
                let own = caller.plugin.as_ref().map(|(id, _)| id.as_str());
                if own != Some(plugin.as_str()) {
                    return Err(WireError::new(
                        "permission_denied",
                        "a plugin in restricted (sandboxed) mode can run only its own actions",
                    ));
                }
                let s = limits::settings(&plugin);
                if s.isolate != herdr::settings::Isolate::Sandbox || s.error.is_some() {
                    return Err(WireError::new(
                        "permission_denied",
                        format!(
                            "{plugin} is no longer configured isolate = \"sandbox\"; a restricted invocation cannot start it"
                        ),
                    ));
                }
            }
            // Herdr: the current context merged with the request's `context`; the
            // correlation id defaults to the invocation's log id (Herdr uses the request id).
            let ctx = InvokeContext::from_params(server, &sn, p, "api", "");
            let log = invoke_action(
                server,
                caller.ctx.pane_scope.as_deref(),
                &plugin,
                &action,
                ctx,
                "api",
            )?;
            // Herdr `plugin_action_invoked {action: PluginActionInfo, context, log}`; the
            // action's `command` is the manifest argv, as in `plugin.action.list`.
            let info = herdr_action_list(Some(&plugin))?
                .into_iter()
                .find(|a| a["action_id"] == action.as_str())
                .unwrap_or_else(|| {
                    json!({"plugin_id": plugin, "action_id": action, "title": action, "command": log.get("command").cloned().unwrap_or_else(|| json!([]))})
                });
            let context = log.get("context").cloned().unwrap_or_else(|| json!({}));
            Ok(typed(
                "plugin_action_invoked",
                json!({"action": info, "context": context, "log": log}),
            ))
        }
        "plugin.log.list" => {
            // A restricted plugin sees its own records only.
            let filter = if caller.sandboxed {
                caller.plugin.as_ref().map(|(id, _)| id.as_str())
            } else {
                sp(p, "plugin_id")
            };
            Ok(typed(
                "plugin_log_list",
                json!({"logs": logs(server, filter, p.get("limit").and_then(Value::as_u64))}),
            ))
        }
        m if gap::METHODS.contains(&m) => gap::call(server, caller, &sn, m, p).await,
        m if rest::METHODS.contains(&m) => rest::call(server, caller, &sn, m, p).await,
        m if ext::METHODS.contains(&m) => ext::call(server, caller, &sn, m, p).await,
        other => match inventory::method_status(other) {
            Some(_) => Err(WireError::new(
                "unsupported",
                format!(
                    "{other} is part of the Herdr {} surface but Vibeke does not serve it: {}",
                    herdr::BASELINE_VERSION,
                    inventory::method_note(other)
                        .unwrap_or("see docs/herdr-compat-inventory.md")
                        .trim_start_matches("`unsupported`: ")
                ),
            )),
            None => Err(wire::retired_method(other).unwrap_or_else(|| wire::unknown_method(other))),
        },
    }
}

/// Herdr `workspace.move_block {workspace_ids, before_workspace_id?}`: move the listed
/// workspaces, in the listed order, as one block before `before_workspace_id` (else to the
/// end); answers `workspace_list`. Vibeke moves them one at a time, so subscribers see
/// `workspace.moved` per workspace that changed place (Herdr: one `workspace.reordered`).
async fn workspace_move_block(
    server: &Arc<Server>,
    caller: &Caller,
    sn: &Snap,
    p: &Value,
) -> Result<Value, WireError> {
    let failed = |m: String| WireError::new("workspace_move_block_failed", m);
    let ids = p
        .get("workspace_ids")
        .and_then(Value::as_array)
        .ok_or_else(|| WireError::new("invalid_params", "workspace_ids is required"))?;
    if ids.is_empty() {
        return Err(failed("workspace_ids must not be empty".into()));
    }
    let not_found =
        |id: &str| WireError::new("workspace_not_found", format!("workspace {id} not found"));
    let mut block: Vec<String> = Vec::new();
    for v in ids {
        let id = v
            .as_str()
            .ok_or_else(|| WireError::new("invalid_params", "workspace_ids must be strings"))?;
        let w = sn.ws(id).ok_or_else(|| not_found(id))?;
        if block.contains(&w.id) {
            return Err(failed(format!("workspace {id} appears more than once")));
        }
        block.push(w.id.clone());
    }
    let before = match sp(p, "before_workspace_id") {
        Some(id) => {
            let w = sn.ws(id).ok_or_else(|| not_found(id))?;
            if block.contains(&w.id) {
                return Err(failed(
                    "before_workspace_id must not be part of workspace_ids".into(),
                ));
            }
            Some(w.id.clone())
        }
        None => None,
    };
    let cur: Vec<String> = sn.ws.iter().map(|w| w.id.clone()).collect();
    let mut want: Vec<String> = cur
        .iter()
        .filter(|w| !block.contains(*w))
        .cloned()
        .collect();
    let at = before
        .as_ref()
        .and_then(|b| want.iter().position(|w| w == b))
        .unwrap_or(want.len());
    for (i, id) in block.iter().enumerate() {
        want.insert(at + i, id.clone());
    }
    if caller.ctx.pane_scope.is_some() {
        return Err(WireError::new(
            "permission_denied",
            "agents can't reorder the user's workspaces",
        ));
    }
    // One commit and one event, as Herdr's `move_workspace_block` (no event when nothing
    // moved).
    if want != cur {
        let mut c = server.core.lock().unwrap();
        let mut tx = crate::core::Tx::new();
        for (k, id) in want.iter().enumerate() {
            if let Some(mut w) = c.ws(id).cloned() {
                w.order = k as f64 + 1.0;
                tx.ws(w);
            }
        }
        let handle = |id: &String| sn.ws(id).map(|w| w.handle.clone());
        let mut data = json!({
            "index": want.iter().position(|w| w == &block[0]).unwrap_or(0),
            "workspace_ids": block.iter().filter_map(handle).collect::<Vec<_>>(),
        });
        if let Some(b) = before.as_ref().and_then(handle) {
            data["before_workspace_id"] = json!(b);
        }
        tx.event("workspace.moved", json!({"workspace": block[0]}), data);
        server
            .commit(&mut c, tx)
            .map_err(|e| WireError::new("internal_error", e.to_string()))?;
    }
    let sn = snap(server);
    Ok(typed(
        "workspace_list",
        json!({"workspaces": sn.ws.iter().map(|w| sn.ws_json(w)).collect::<Vec<_>>()}),
    ))
}

/// One Herdr `pane.output_matched` subscription with a matcher (Herdr
/// `ActiveOutputMatchedSubscription`).
struct OutputWatcher {
    pane: String,
    handle: String,
    workspace: String,
    tab: String,
    watch: hev::OutputWatch,
    re: Option<regex::Regex>,
    /// The output revision last read; the pane is read again only when it changed.
    rev: Option<u64>,
    /// A line matched at the last read: the next event needs a read without a match first.
    matching: bool,
}

/// The output watchers of `subs`; `pane_not_found` or `invalid_regex` refuse the subscription.
fn output_watchers(
    server: &Server,
    subs: &[hev::Subscription],
) -> Result<Vec<OutputWatcher>, WireError> {
    let sn = snap(server);
    let mut out = Vec::new();
    for sub in subs {
        let (Some(w), Some(id)) = (&sub.output, &sub.pane_id) else {
            continue;
        };
        let x = sn
            .pane(id)
            .ok_or_else(|| WireError::new("pane_not_found", format!("pane {id} not found")))?;
        let re = if w.regex {
            Some(
                regex::Regex::new(&w.value)
                    .map_err(|e| WireError::new("invalid_regex", e.to_string()))?,
            )
        } else {
            None
        };
        out.push(OutputWatcher {
            pane: x.id.clone(),
            handle: x.handle.clone(),
            workspace: sn.ws_handle(&x.workspace).unwrap_or_default(),
            tab: sn.tab_handle(&x.tab).unwrap_or_default(),
            watch: w.clone(),
            re,
            rev: None,
            matching: false,
        });
    }
    Ok(out)
}

/// Read a watched pane when its output changed; Herdr's `{pane_id, matched_line, read}` when a
/// line starts to match (once per run of matching reads, as Herdr does).
async fn poll_watcher(
    server: &Arc<Server>,
    caller: &Caller,
    w: &mut OutputWatcher,
) -> Option<Value> {
    let rev = server.pane_rt(&w.pane).map(|r| r.rev())?;
    if w.rev == Some(rev) {
        return None;
    }
    w.rev = Some(rev);
    let mut np = json!({"pane": w.pane, "source": w.watch.source});
    np["lines"] = json!(w.watch.lines.unwrap_or(50));
    let r = native(server, caller, "pane.read", np).await.ok()?;
    let text = r["text"].as_str().unwrap_or("");
    let substring = w.re.is_none().then_some(w.watch.value.as_str());
    let Some(line) = match_line(text, substring, w.re.as_ref()) else {
        w.matching = false;
        return None;
    };
    if std::mem::replace(&mut w.matching, true) {
        return None;
    }
    Some(json!({
        "pane_id": w.handle,
        "matched_line": line,
        "read": {
            "pane_id": w.handle,
            "workspace_id": w.workspace,
            "tab_id": w.tab,
            "source": w.watch.source,
            "format": "text",
            "text": text,
            "revision": r["revision"].as_u64().unwrap_or(rev),
            "truncated": false,
        },
    }))
}

/// Herdr's `limit_snapshot_lines`: the last `limit` lines, `truncated` when lines were left
/// out.
fn limit_lines(text: &str, limit: Option<usize>) -> (String, bool) {
    let Some(limit) = limit else {
        return (text.to_string(), false);
    };
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    (
        lines[lines.len().saturating_sub(limit)..].concat(),
        lines.len() > limit,
    )
}

/// A pane's rows as ANSI text (SGR styles, as `pane.screenshot` renders them): the visible
/// screen, or the last `lines` rendered rows of history and screen (soft wraps joined for
/// `recent_unwrapped`); trailing blank rows dropped.
fn ansi_rows(server: &Server, pane: &str, source: &str, lines: usize) -> String {
    let Some(rt) = server.pane_rt(pane) else {
        return String::new();
    };
    let sc = rt.screen.lock().unwrap();
    let e = &sc.engine;
    let mut rows: Vec<vk_proto::render::Row> = Vec::new();
    if source != "visible" {
        let h = e.history_len();
        let want = lines.saturating_sub(e.rows() as usize).min(h);
        rows.extend((h - want..h).filter_map(|i| e.history_row(i)));
    }
    rows.extend(e.visible_rows());
    drop(sc);
    while rows.last().is_some_and(|r| r.text().trim().is_empty()) {
        rows.pop();
    }
    if source != "visible" {
        rows.drain(..rows.len().saturating_sub(lines));
    }
    let unwrap = source == "recent_unwrapped";
    let mut out = String::new();
    for (i, r) in rows.iter().enumerate() {
        out.push_str(&crate::pane_api::rows_ansi(std::slice::from_ref(r)));
        if !(r.wrapped && unwrap) && i + 1 < rows.len() {
            out.push('\n');
        }
    }
    out
}

/// Herdr `pane.read` (`read_terminal_snapshot`): `source` `visible` | `recent` |
/// `recent_unwrapped` | `detection`, `format` `text` | `ansi` (`detection` is always text),
/// `lines` (at most 1000; `recent` reads default to 80). `truncated` as Herdr: a `lines` limit
/// left lines out of the visible text, or the terminal has more rows than a recent read
/// returns. Herdr reports `revision` 0.
async fn herdr_read(
    server: &Arc<Server>,
    caller: &Caller,
    sn: &Snap,
    x: &Pane,
    p: &Value,
) -> Result<Value, WireError> {
    let source = sp(p, "source").unwrap_or("recent");
    if !matches!(
        source,
        "visible" | "recent" | "recent_unwrapped" | "detection"
    ) {
        return Err(wire::invalid_request(format!(
            "unknown variant `{source}`, expected one of `visible`, `recent`, `recent_unwrapped`, `detection`"
        )));
    }
    let format = sp(p, "format").unwrap_or("text");
    if !matches!(format, "text" | "ansi") {
        return Err(wire::invalid_request(format!(
            "unknown variant `{format}`, expected `text` or `ansi`"
        )));
    }
    let limit = p
        .get("lines")
        .and_then(Value::as_u64)
        .map(|n| n.min(1000) as usize);
    let recent = limit.unwrap_or(80);
    let screen = matches!(source, "visible" | "detection");
    // The native read checks the caller may read the pane (and redacts for remote callers).
    let r = native(
        server,
        caller,
        "pane.read",
        json!({"pane": x.id, "source": source, "lines": if screen { 1000 } else { recent }}),
    )
    .await?;
    let plain = r["text"].as_str().unwrap_or("");
    let ansi = format == "ansi" && source != "detection" && !caller.ctx.remote;
    let (text, truncated) = if screen {
        let full = if ansi {
            ansi_rows(server, &x.id, source, 0)
        } else {
            plain.to_string()
        };
        limit_lines(&full, limit)
    } else {
        let total = server
            .pane_rt(&x.id)
            .map(|rt| {
                let sc = rt.screen.lock().unwrap();
                sc.engine.history_len() + usize::from(sc.engine.rows())
            })
            .unwrap_or(0);
        let text = if ansi {
            ansi_rows(server, &x.id, source, recent)
        } else {
            plain.to_string()
        };
        (text, total > recent)
    };
    Ok(json!({
        "pane_id": x.handle,
        "workspace_id": sn.ws_handle(&x.workspace).unwrap_or_default(),
        "tab_id": sn.tab_handle(&x.tab).unwrap_or_default(),
        "source": source,
        "format": format,
        "text": text,
        "revision": 0,
        "truncated": truncated,
    }))
}

/// Herdr's `match_output`: the first line containing the substring or matching the regex.
fn match_line(text: &str, substring: Option<&str>, re: Option<&regex::Regex>) -> Option<String> {
    text.lines()
        .find(|l| match (substring, re) {
            (Some(s), _) => l.contains(s),
            (None, Some(re)) => re.is_match(l),
            (None, None) => false,
        })
        .map(str::to_string)
}

/// Herdr `pane.wait_for_output {pane_id, source, lines?, match: {type: substring|regex,
/// value}, strip_ansi?, timeout_ms?}`: reads the pane (`recent` reads `recent_unwrapped`, as
/// Herdr does) until a line matches; `output_matched {pane_id, revision, matched_line, read}`.
/// `invalid_regex`; `timeout` only with `timeout_ms` (Herdr waits without a deadline). Vibeke
/// also accepts `match` as a plain string and the older `regex`/`pattern`/`text`.
async fn wait_for_output(
    server: &Arc<Server>,
    caller: &Caller,
    sn: &Snap,
    x: &Pane,
    p: &Value,
) -> Result<Value, WireError> {
    let (kind, value) = match p.get("match") {
        Some(Value::Object(m)) => (
            m.get("type").and_then(Value::as_str).unwrap_or(""),
            m.get("value")
                .and_then(Value::as_str)
                .ok_or_else(|| WireError::new("invalid_params", "match.value is required"))?,
        ),
        Some(Value::String(v)) => ("substring", v.as_str()),
        _ => match (sp(p, "regex"), sp(p, "pattern").or(sp(p, "text"))) {
            (Some(r), _) => ("regex", r),
            (None, Some(v)) => ("substring", v),
            _ => {
                return Err(WireError::new("invalid_params", "match is required"));
            }
        },
    };
    let re = match kind {
        "regex" => Some(
            regex::Regex::new(value).map_err(|e| WireError::new("invalid_regex", e.to_string()))?,
        ),
        "substring" => None,
        other => {
            return Err(WireError::new(
                "invalid_params",
                format!("unknown match type `{other}`"),
            ));
        }
    };
    let substring = re.is_none().then_some(value);
    let source = match sp(p, "source").unwrap_or("recent") {
        "recent" => "recent_unwrapped",
        s => s,
    };
    let mut rp = p.clone();
    rp["source"] = json!(source);
    // Herdr's wait reads text, whatever `format` the request carries.
    rp["format"] = json!("text");
    let deadline = p
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .map(|ms| tokio::time::Instant::now() + Duration::from_millis(ms));
    loop {
        let read = herdr_read(server, caller, sn, x, &rp).await?;
        if let Some(line) = match_line(read["text"].as_str().unwrap_or(""), substring, re.as_ref())
        {
            // Registered matchers feed the `pane.output_matched` event (07 §8.3).
            {
                let mut c = server.core.lock().unwrap();
                let mut tx = crate::core::Tx::new();
                tx.event(
                    "pane.output_matched",
                    crate::core::subject_pane(x),
                    json!({"matched": line, "revision": read["revision"]}),
                );
                let _ = server.commit(&mut c, tx);
            }
            return Ok(typed(
                "output_matched",
                json!({
                    "pane_id": x.handle,
                    "revision": read["revision"],
                    "matched_line": line,
                    "read": read,
                }),
            ));
        }
        if deadline.is_some_and(|d| tokio::time::Instant::now() >= d) {
            return Err(WireError::new(
                "timeout",
                "timed out waiting for output match",
            ));
        }
        check_broker(server, caller)?;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Herdr `pane.resize {pane_id?, direction, amount?}` (`amount` a share of the tab, default
/// 0.05, at most 0.5): `pane_resize {resize: {changed, reason?: unchanged, pane_id,
/// focused_pane_id, layout}}`. Vibeke's `percent` is accepted too.
async fn pane_resize(
    server: &Arc<Server>,
    caller: &Caller,
    sn: &Snap,
    p: &Value,
) -> Result<Value, WireError> {
    let id = pane_target(caller, sn, p)?;
    let x = sn
        .pane(&id)
        .cloned()
        .ok_or_else(|| WireError::new("pane_not_found", format!("pane {id} not found")))?;
    let direction = req_s(p, "direction")?;
    if !matches!(direction, "left" | "right" | "up" | "down") {
        return Err(WireError::new(
            "invalid_params",
            format!("unknown direction `{direction}`"),
        ));
    }
    let percent = match (
        p.get("amount").and_then(Value::as_f64),
        p.get("percent").and_then(Value::as_f64),
    ) {
        (Some(a), _) if a.is_finite() => a.abs().min(0.5) * 100.0,
        (_, Some(pc)) => pc,
        _ => 5.0,
    };
    let before = sn.tab(&x.tab).map(|t| json!(t.layout));
    native(
        server,
        caller,
        "pane.resize",
        json!({"pane": x.id, "direction": direction, "percent": percent}),
    )
    .await?;
    let sn = snap(server);
    let t = sn
        .tab(&x.tab)
        .ok_or_else(|| WireError::new("pane_layout_unavailable", "pane layout unavailable"))?;
    let changed = before.as_ref() != Some(&json!(t.layout));
    let layout = ext::tab_snapshot(&sn, t);
    let mut r = json!({"changed": changed});
    if !changed {
        r["reason"] = json!("unchanged");
    }
    r["pane_id"] = json!(x.handle);
    r["focused_pane_id"] = layout["focused_pane_id"].clone();
    r["layout"] = layout;
    Ok(typed("pane_resize", json!({"resize": r})))
}

/// Herdr `pane.zoom {pane_id?, mode: toggle|on|off}`: focuses the pane, then zooms or unzooms
/// its tab; `pane_zoom {zoom: {changed, zoom_changed, focus_changed, reason?, pane_id,
/// focused_pane_id, zoomed, layout}}` with `single_pane`, `already_zoomed`,
/// `already_unzoomed`. Vibeke's `zoomed: bool` is read as `on`/`off`.
async fn pane_zoom(
    server: &Arc<Server>,
    caller: &Caller,
    sn: &Snap,
    p: &Value,
) -> Result<Value, WireError> {
    let id = pane_target(caller, sn, p)?;
    let x = sn
        .pane(&id)
        .cloned()
        .ok_or_else(|| WireError::new("pane_not_found", format!("pane {id} not found")))?;
    let mode = match (sp(p, "mode"), p.get("zoomed").and_then(Value::as_bool)) {
        (Some(m @ ("toggle" | "on" | "off")), _) => m,
        (Some(m), _) => {
            return Err(WireError::new(
                "invalid_params",
                format!("unknown zoom mode `{m}`"),
            ));
        }
        (None, Some(true)) => "on",
        (None, Some(false)) => "off",
        (None, None) => "toggle",
    };
    let before = sn.focus.pane.clone();
    // Herdr focuses the pane first; a caller that may not move the focus keeps it.
    let _ = native(server, caller, "pane.focus", json!({"pane": x.id})).await;
    let sn = snap(server);
    let focus_changed = sn.focus.pane != before;
    let t = sn
        .tab(&x.tab)
        .cloned()
        .ok_or_else(|| WireError::new("pane_layout_unavailable", "pane layout unavailable"))?;
    let zoomed = t.zoomed_pane.is_some();
    let (zoom_changed, reason, now) = if t.layout.panes().len() <= 1 {
        (false, Some("single_pane"), zoomed)
    } else {
        match (mode, zoomed) {
            ("on", true) => (false, Some("already_zoomed"), true),
            ("off", false) => (false, Some("already_unzoomed"), false),
            _ => {
                let want = match mode {
                    "on" => true,
                    "off" => false,
                    _ => !zoomed,
                };
                native(
                    server,
                    caller,
                    "pane.zoom",
                    json!({"pane": x.id, "zoomed": want}),
                )
                .await?;
                (true, None, want)
            }
        }
    };
    let sn = snap(server);
    let t = sn
        .tab(&x.tab)
        .ok_or_else(|| WireError::new("pane_layout_unavailable", "pane layout unavailable"))?;
    let layout = ext::tab_snapshot(&sn, t);
    let mut z = json!({
        "changed": zoom_changed || focus_changed,
        "zoom_changed": zoom_changed,
        "focus_changed": focus_changed,
    });
    if let Some(r) = reason {
        z["reason"] = json!(r);
    }
    z["pane_id"] = json!(x.handle);
    z["focused_pane_id"] = layout["focused_pane_id"].clone();
    z["zoomed"] = json!(now);
    z["layout"] = layout;
    Ok(typed("pane_zoom", json!({"zoom": z})))
}

// ---- worktrees --------------------------------------------------------------------------------

/// Herdr's `absolute_user_path`: `~` expands to the home directory; the path must be absolute.
fn user_path(path: &str) -> Result<String, WireError> {
    let home = crate::paths::home();
    let p = if path == "~" {
        home
    } else if let Some(rest) = path.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(path)
    };
    if p.is_absolute() {
        Ok(p.to_string_lossy().into_owned())
    } else {
        Err(WireError::new(
            "invalid_request",
            "worktree path must be absolute",
        ))
    }
}

fn canon_path(s: &str) -> PathBuf {
    std::fs::canonicalize(s).unwrap_or_else(|_| PathBuf::from(s))
}

/// The workspace rooted at `path` (compared canonically), as Herdr's
/// `open_workspace_idx_for_checkout`.
fn ws_at<'a>(sn: &'a Snap, path: &str) -> Option<&'a Workspace> {
    let c = canon_path(path);
    sn.ws.iter().find(|w| canon_path(&w.root_path) == c)
}

/// `git <git_args> <args>` with Git's English diagnostics; `Err` carries Git's message.
fn run_git(repo: &str, trust: bool, args: &[String]) -> Result<std::process::Output, String> {
    let mut all = herdr::worktree::git_args(Path::new(repo), trust);
    all.extend(args.iter().cloned());
    std::process::Command::new("git")
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(&all)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| e.to_string())
}

fn git_failure(out: &std::process::Output, what: &str) -> String {
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    match (stderr.is_empty(), stdout.is_empty()) {
        (false, _) => stderr,
        (true, false) => stdout,
        (true, true) => format!("{what} failed with status {}", out.status),
    }
}

/// Herdr's `list_existing_worktrees` (`trust_repository` adds `safe.directory`).
async fn git_worktrees(
    repo: &str,
    trust: bool,
) -> Result<Vec<herdr::worktree::ExistingWorktree>, WireError> {
    let repo = repo.to_string();
    let out = tokio::task::spawn_blocking(move || {
        run_git(
            &repo,
            trust,
            &["worktree".into(), "list".into(), "--porcelain".into()],
        )
    })
    .await
    .map_err(|e| WireError::new("worktree_list_failed", e.to_string()))?
    .map_err(|e| WireError::new("worktree_list_failed", e))?;
    if !out.status.success() {
        return Err(WireError::new(
            "worktree_list_failed",
            git_failure(&out, "git worktree list"),
        ));
    }
    Ok(herdr::worktree::parse_porcelain(&String::from_utf8_lossy(
        &out.stdout,
    )))
}

/// Herdr's `WorktreeSource`: the repository a worktree request starts from.
struct WtSource {
    /// The workspace the request named (or the active one) when it is the repository's parent.
    workspace: Option<String>,
    /// The main checkout (`source_checkout_path` and `repo_root`).
    checkout: String,
    repo_key: Value,
    repo_name: String,
}

/// Herdr's source resolution: `workspace_id`, else `cwd` (absolute, `~` expanded), else the
/// active workspace; both is `invalid_request`. A linked worktree is refused unless
/// `allow_linked` (`worktree.list`, which then lists from its main checkout).
async fn worktree_source(
    server: &Arc<Server>,
    caller: &Caller,
    sn: &Snap,
    p: &Value,
    allow_linked: bool,
) -> Result<WtSource, WireError> {
    let (ws_given, cwd) = (sp(p, "workspace_id"), sp(p, "cwd"));
    if ws_given.is_some() && cwd.is_some() {
        return Err(WireError::new(
            "invalid_request",
            "only one of workspace_id or cwd may be supplied",
        ));
    }
    let (from_ws, dir) = match (ws_given, cwd) {
        (_, Some(c)) => (None, user_path(c)?),
        (Some(id), None) => {
            let w = sn.ws(id).ok_or_else(|| {
                WireError::new("workspace_not_found", format!("workspace {id} not found"))
            })?;
            (Some(w.clone()), w.root_path.clone())
        }
        (None, None) => {
            let w = sn.active_ws().ok_or_else(|| {
                WireError::new(
                    "invalid_request",
                    "workspace_id or cwd is required when no workspace is active",
                )
            })?;
            (Some(w.clone()), w.root_path.clone())
        }
    };
    let not_git = || {
        WireError::new(
            "not_git_worktree",
            if from_ws.is_some() {
                "Herdr worktree actions require a workspace inside a Git work tree"
            } else {
                "Herdr worktree actions require a path inside a Git work tree"
            },
        )
    };
    let root = native(server, caller, "worktree.repo_root", json!({"cwd": dir}))
        .await
        .map_err(|_| not_git())?;
    let checkout = root["worktree_root"]
        .as_str()
        .or(root["repo_root"].as_str())
        .ok_or_else(not_git)?
        .to_string();
    let space = git_space(&checkout).ok_or_else(not_git)?;
    let linked = space["is_linked_worktree"] == true;
    if linked && !allow_linked {
        return Err(WireError::new(
            "linked_worktree_source",
            "New and open worktree actions start from the repo parent workspace.",
        ));
    }
    let main = if linked {
        space["repo_root"].as_str().unwrap_or(&checkout).to_string()
    } else {
        checkout.clone()
    };
    let workspace = from_ws
        .filter(|_| !linked)
        .map(|w| w.handle)
        .or_else(|| ws_at(sn, &main).map(|w| w.handle.clone()));
    Ok(WtSource {
        workspace,
        checkout: main,
        repo_key: space["repo_key"].clone(),
        repo_name: space["repo_name"].as_str().unwrap_or("").to_string(),
    })
}

/// Herdr's `WorktreeInfo` for one listed entry (`worktree_info_for_entry`).
fn wt_entry_info(sn: &Snap, src: &WtSource, e: &herdr::worktree::ExistingWorktree) -> Value {
    let path = e.path.to_string_lossy().into_owned();
    let mut v = Map::new();
    v.insert("path".into(), json!(path));
    if let Some(b) = &e.branch {
        v.insert("branch".into(), json!(b));
    }
    v.insert("is_bare".into(), json!(e.is_bare));
    v.insert("is_detached".into(), json!(e.is_detached));
    v.insert("is_prunable".into(), json!(e.is_prunable));
    v.insert(
        "is_linked_worktree".into(),
        json!(canon_path(&path) != canon_path(&src.checkout)),
    );
    if let Some(w) = ws_at(sn, &path) {
        v.insert("open_workspace_id".into(), json!(w.handle));
    }
    v.insert("label".into(), json!(src.repo_name));
    Value::Object(v)
}

/// Herdr `worktree.list {workspace_id? | cwd?, trust_repository?}`: the repository of the
/// workspace (else the active one) or of `cwd`, as `worktree_list {source: WorktreeSourceInfo,
/// worktrees: [WorktreeInfo]}`. A linked worktree lists from its main checkout.
async fn worktree_list(
    server: &Arc<Server>,
    caller: &Caller,
    sn: &Snap,
    p: &Value,
) -> Result<Value, WireError> {
    let src = worktree_source(server, caller, sn, p, true).await?;
    let trust = p
        .get("trust_repository")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let entries = git_worktrees(&src.checkout, trust).await?;
    let mut source = json!({
        "repo_key": src.repo_key,
        "repo_name": src.repo_name,
        "repo_root": src.checkout,
        "source_checkout_path": src.checkout,
    });
    if let Some(w) = &src.workspace {
        source["source_workspace_id"] = json!(w);
    }
    let worktrees: Vec<Value> = entries.iter().map(|e| wt_entry_info(sn, &src, e)).collect();
    Ok(typed(
        "worktree_list",
        json!({"source": source, "worktrees": worktrees}),
    ))
}

/// The active tab of a workspace (the focused one, else its first) and that tab's root pane,
/// as Herdr reports them for an opened or created worktree workspace.
fn ws_tab_root(server: &Server, sn: &Snap, ws_id: &str) -> (Value, Value) {
    let Some(w) = sn.ws(ws_id) else {
        return (Value::Null, Value::Null);
    };
    let tab = sn.active_tab(w);
    let root = tab
        .and_then(|t| t.layout.panes().first().cloned())
        .and_then(|x| sn.pane(&x).map(|x| sn.pane_json(server, x)));
    (
        tab.map(|t| sn.tab_json(t)).unwrap_or(Value::Null),
        root.unwrap_or(Value::Null),
    )
}

/// Herdr's `ensure_source_parent_membership`: the repository's main checkout gets a workspace
/// (not focused) when none is open there yet.
fn ensure_parent_workspace(server: &Arc<Server>, src: &WtSource) -> Result<(), WireError> {
    if ws_at(&snap(server), &src.checkout).is_some() {
        return Ok(());
    }
    server
        .create_workspace(&src.checkout, None, None, None)
        .map(|_| ())
        .map_err(|e| WireError::new("worktree_open_failed", format!("{e:#}")))
}

fn focus_allowed(caller: &Caller, focus: bool) -> Result<(), WireError> {
    if focus && caller.ctx.pane_scope.is_some() {
        return Err(WireError::new(
            "permission_denied",
            "agents can't move the user's focus",
        ));
    }
    Ok(())
}

/// Herdr `worktree.open {path | branch, workspace_id? | cwd?, label?, focus?,
/// trust_repository?}`: finds the worktree among the repository's worktrees (by path, or the
/// one non-bare, non-prunable, attached checkout of `branch`), then reuses the workspace open
/// there or opens one. `worktree_opened {workspace, tab, root_pane, worktree, already_open}`;
/// emits `worktree.opened` (and `workspace.created` for a new workspace).
async fn herdr_worktree_open(
    server: &Arc<Server>,
    caller: &Caller,
    sn: &Snap,
    p: &Value,
) -> Result<Value, WireError> {
    let (path, branch) = (sp(p, "path"), sp(p, "branch"));
    if path.is_some() == branch.is_some() {
        return Err(WireError::new(
            "invalid_request",
            "exactly one of path or branch is required",
        ));
    }
    let focus = p.get("focus").and_then(Value::as_bool).unwrap_or(false);
    focus_allowed(caller, focus)?;
    let trust = p
        .get("trust_repository")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let src = worktree_source(server, caller, sn, p, false).await?;
    let entries = git_worktrees(&src.checkout, trust).await?;
    let entry = match (path, branch) {
        (Some(path), _) => {
            let want = canon_path(&user_path(path)?);
            entries
                .into_iter()
                .find(|e| canon_path(&e.path.to_string_lossy()) == want)
                .ok_or_else(|| WireError::new("worktree_not_found", "worktree path not found"))?
        }
        (None, Some(branch)) => {
            let mut m: Vec<_> = entries
                .into_iter()
                .filter(|e| {
                    !e.is_bare
                        && !e.is_prunable
                        && !e.is_detached
                        && e.branch.as_deref() == Some(branch)
                })
                .collect();
            match m.len() {
                0 => {
                    return Err(WireError::new(
                        "worktree_not_found",
                        "worktree branch not found",
                    ));
                }
                1 => m.remove(0),
                _ => {
                    return Err(WireError::new(
                        "ambiguous_worktree_branch",
                        "multiple worktrees matched branch",
                    ));
                }
            }
        }
        (None, None) => unreachable!("checked above"),
    };
    if entry.is_bare || entry.is_prunable {
        return Err(WireError::new(
            "worktree_not_found",
            "worktree cannot be opened",
        ));
    }
    let target = entry.path.to_string_lossy().into_owned();
    if canon_path(&target) != canon_path(&src.checkout) {
        ensure_parent_workspace(server, &src)?;
    }
    let label = sp(p, "label").map(str::to_string);
    let mut np = json!({"path": target, "focus": focus});
    if let Some(l) = &label {
        np["name"] = json!(l);
    }
    let r = native(server, caller, "worktree.open", np).await?;
    let created = r["created"].as_bool().unwrap_or(false);
    let ws_id = r["workspace"]["id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    if let (Some(l), false) = (&label, created) {
        native(
            server,
            caller,
            "workspace.rename",
            json!({"workspace": ws_id, "name": l}),
        )
        .await?;
    }
    let sn = snap(server);
    let (tab, root) = ws_tab_root(server, &sn, &ws_id);
    Ok(typed(
        "worktree_opened",
        json!({
            "workspace": sn.ws(&ws_id).map(|w| sn.ws_json(w)),
            "tab": tab,
            "root_pane": root,
            "worktree": wt_entry_info(&sn, &src, &entry),
            "already_open": !created,
            "created": created,
        }),
    ))
}

/// Herdr `worktree.create {branch?, base?, path?, workspace_id? | cwd?, label?, focus?,
/// trust_repository?}`: `git worktree add` from the repository's main checkout (an existing
/// local branch is checked out, otherwise `-b <branch>` from `base`, default `HEAD`), then a
/// workspace opens at the checkout. Without `branch` Herdr's generated `worktree/<name>` is
/// used; without `path` the checkout goes to `<[tasks] root>/<repo>/<branch slug>` (Herdr:
/// `[worktrees] directory`). `worktree_created {workspace, tab, root_pane, worktree}`; emits
/// `worktree.created`.
async fn herdr_worktree_create(
    server: &Arc<Server>,
    caller: &Caller,
    sn: &Snap,
    p: &Value,
) -> Result<Value, WireError> {
    let branch = match sp(p, "branch") {
        Some(b) => b.trim().to_string(),
        None => herdr::worktree::generated_branch(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_micros().min(u128::from(u64::MAX)) as u64)
                .unwrap_or(0),
        ),
    };
    if branch.is_empty() {
        return Err(WireError::new("invalid_request", "branch is required"));
    }
    let base = sp(p, "base").unwrap_or("HEAD").to_string();
    let focus = p.get("focus").and_then(Value::as_bool).unwrap_or(false);
    focus_allowed(caller, focus)?;
    let trust = p
        .get("trust_repository")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let src = worktree_source(server, caller, sn, p, false).await?;
    let path = match sp(p, "path") {
        Some(x) => user_path(x)?,
        None => {
            let cfg = crate::run::worktree_cfg(&json!({}));
            vk_tasks::worktree_path(
                &cfg.root,
                Path::new(&src.checkout),
                &herdr::worktree::branch_slug(&branch),
            )
            .to_string_lossy()
            .into_owned()
        }
    };
    {
        let (repo, path, branch, base) = (
            src.checkout.clone(),
            PathBuf::from(&path),
            branch.clone(),
            base.clone(),
        );
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let exists = run_git(
                &repo,
                trust,
                &[
                    "show-ref".into(),
                    "--verify".into(),
                    "--quiet".into(),
                    format!("refs/heads/{branch}"),
                ],
            )?
            .status
            .success();
            let out = run_git(
                &repo,
                trust,
                &herdr::worktree::add_args(&path, &branch, &base, exists),
            )?;
            if out.status.success() {
                Ok(())
            } else {
                Err(git_failure(&out, "git worktree add"))
            }
        })
        .await
        .map_err(|e| WireError::new("worktree_create_failed", e.to_string()))?
        .map_err(|e| WireError::new("worktree_create_failed", e))?;
    }
    ensure_parent_workspace(server, &src)?;
    let label = sp(p, "label").map(str::to_string);
    let open_fail = |e: anyhow::Error| {
        WireError::new(
            "worktree_open_failed",
            format!("created worktree but failed to open workspace: {e:#}"),
        )
    };
    let existing = ws_at(&snap(server), &path).cloned();
    let ws_id = match existing {
        Some(w) => {
            if focus {
                native(
                    server,
                    caller,
                    "workspace.focus",
                    json!({"workspace": w.id}),
                )
                .await?;
            }
            if let Some(l) = &label {
                native(
                    server,
                    caller,
                    "workspace.rename",
                    json!({"workspace": w.id, "name": l}),
                )
                .await?;
            }
            w.id
        }
        None => {
            let focus_client = focus.then_some(caller.ctx.client_id.as_str());
            server
                .create_workspace(&path, label.clone(), None, focus_client)
                .map_err(open_fail)?
                .0
                .id
        }
    };
    {
        let mut c = server.core.lock().unwrap();
        let mut tx = crate::core::Tx::new();
        tx.event(
            "worktree.created",
            json!({"workspace": ws_id}),
            json!({"path": path, "branch": branch, "base_ref": base, "repo_root": src.checkout}),
        );
        server
            .commit(&mut c, tx)
            .map_err(|e| WireError::new("internal_error", e.to_string()))?;
    }
    let sn = snap(server);
    let (tab, root) = ws_tab_root(server, &sn, &ws_id);
    let entry = herdr::worktree::ExistingWorktree {
        path: PathBuf::from(&path),
        branch: Some(branch),
        is_bare: false,
        is_detached: false,
        is_prunable: false,
    };
    Ok(typed(
        "worktree_created",
        json!({
            "workspace": sn.ws(&ws_id).map(|w| sn.ws_json(w)),
            "tab": tab,
            "root_pane": root,
            "worktree": wt_entry_info(&sn, &src, &entry),
        }),
    ))
}

/// Herdr's `SessionSnapshot`: `version`, the integer `protocol`, the focused ids (left out
/// when unknown), every workspace, tab and live pane, one `PaneLayoutSnapshot` per tab (in
/// workspace then tab order) and every agent.
fn session_snapshot(server: &Server, sn: &Snap) -> Value {
    let (fw, ft, fp) = sn.focused();
    let mut v = Map::new();
    v.insert("version".into(), json!(herdr::BASELINE_VERSION));
    v.insert("protocol".into(), json!(herdr::cli::PROTOCOL_VERSION));
    for (k, x) in [
        ("focused_workspace_id", fw),
        ("focused_tab_id", ft),
        ("focused_pane_id", fp),
    ] {
        if let Some(x) = x {
            v.insert(k.into(), json!(x));
        }
    }
    v.insert(
        "workspaces".into(),
        json!(sn.ws.iter().map(|w| sn.ws_json(w)).collect::<Vec<_>>()),
    );
    // Tabs and layouts in workspace order, as Herdr walks them.
    let tabs: Vec<&Tab> = sn
        .ws
        .iter()
        .flat_map(|w| sn.tabs.iter().filter(move |t| t.workspace == w.id))
        .collect();
    v.insert(
        "tabs".into(),
        json!(tabs.iter().map(|t| sn.tab_json(t)).collect::<Vec<_>>()),
    );
    v.insert(
        "panes".into(),
        json!(
            sn.panes
                .iter()
                .filter(|x| !x.exited)
                .map(|x| sn.pane_json(server, x))
                .collect::<Vec<_>>()
        ),
    );
    v.insert(
        "layouts".into(),
        json!(
            tabs.iter()
                .map(|t| ext::tab_snapshot(sn, t))
                .collect::<Vec<_>>()
        ),
    );
    v.insert(
        "agents".into(),
        json!(
            sn.pane_run
                .values()
                .map(|r| sn.agent_json(server, r))
                .collect::<Vec<_>>()
        ),
    );
    Value::Object(v)
}

fn keys(p: &Value) -> Result<Value, WireError> {
    match p.get("keys") {
        Some(Value::Array(a)) => Ok(Value::Array(a.clone())),
        Some(Value::String(s)) => Ok(json!(s.split_whitespace().collect::<Vec<_>>())),
        _ => Err(WireError::new("invalid_params", "keys is required")),
    }
}

/// Herdr's `pane_agent_status_changed` data for `pane` now: `pane_id`, `workspace_id`,
/// `agent_status` and the presentation fields `PaneInfo` carries.
fn agent_status_data(server: &Server, sn: &Snap, x: &Pane) -> Value {
    let mut d = Map::new();
    d.insert("pane_id".into(), json!(x.handle));
    d.insert(
        "workspace_id".into(),
        json!(sn.ws_handle(&x.workspace).unwrap_or_default()),
    );
    d.insert(
        "agent_status".into(),
        json!(sn.pane_status(&x.id).unwrap_or("unknown")),
    );
    let pj = sn.pane_json(server, x);
    for k in ["agent", "title", "display_agent", "state_labels"] {
        if let Some(v) = pj.get(k) {
            d.insert(k.into(), v.clone());
        }
    }
    Value::Object(d)
}

/// Herdr `events.wait {match_event: {event: "pane_agent_status_changed", pane_id,
/// agent_status}, timeout_ms?}`: `wait_matched {event: EventEnvelope}` as soon as the pane has
/// that status (at once when it already has it). Herdr waits without a deadline when
/// `timeout_ms` is absent; `timeout` with Herdr's message otherwise.
async fn status_wait(server: &Arc<Server>, caller: &Caller, p: &Value) -> Result<Value, WireError> {
    let want = hev::parse_event_match(p).map_err(|(c, m)| WireError::new(c, m))?;
    let matched = |data: Value| {
        typed(
            "wait_matched",
            json!({"event": hev::envelope("pane.agent_status_changed", data)}),
        )
    };
    // Subscribe before reading the current status so no change slips between the two.
    let mut rx = server.events.subscribe();
    let mut proj = seeded_projector(server);
    let sn = snap(server);
    let x = sn.pane(&want.pane_id).cloned().ok_or_else(|| {
        WireError::new("pane_not_found", format!("pane {} not found", want.pane_id))
    })?;
    if sn.pane_status(&x.id).unwrap_or("unknown") == want.agent_status {
        return Ok(matched(agent_status_data(server, &sn, &x)));
    }
    let deadline = want
        .timeout_ms
        .map(|ms| tokio::time::Instant::now() + Duration::from_millis(ms));
    loop {
        let next = match deadline {
            Some(d) => match tokio::time::timeout_at(d, rx.recv()).await {
                Ok(r) => r,
                Err(_) => {
                    return Err(WireError::new(
                        "timeout",
                        "timed out waiting for event match",
                    ));
                }
            },
            None => rx.recv().await,
        };
        let ev = match next {
            Ok(e) => e,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(_) => return Err(WireError::new("internal_error", "event stream closed")),
        };
        check_broker(server, caller)?;
        for (name, pane, data) in project_event(server, &mut proj, &ev) {
            if name == "pane.agent_status_changed"
                && pane.as_deref() == Some(x.handle.as_str())
                && data["agent_status"] == want.agent_status.as_str()
            {
                return Ok(matched(data));
            }
        }
    }
}

async fn events_wait(server: &Arc<Server>, caller: &Caller, p: &Value) -> Result<Value, WireError> {
    if caller.cross_session {
        return Err(WireError::new(
            "unsupported",
            "events.wait is not available to plugins across sessions",
        ));
    }
    // Herdr 0.9.3: `{match_event, timeout_ms?}`; Vibeke's older `{subscriptions}` form stays.
    if p.get("match_event").is_some() || p.get("subscriptions").is_none() {
        return status_wait(server, caller, p).await;
    }
    let subs = hev::parse_subscriptions(p).map_err(|(c, m)| WireError::new(c, m))?;
    let timeout = Duration::from_millis(
        p.get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(30_000),
    );
    let mut rx = server.events.subscribe();
    let mut proj = seeded_projector(server);
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let ev = match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Ok(e)) => e,
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(_)) => return Err(WireError::new("internal_error", "event stream closed")),
            Err(_) => return Err(WireError::new("timeout", "no matching event")),
        };
        // The wait may have outlived the invocation or its grant.
        check_broker(server, caller)?;
        for (name, pane, data) in project_event(server, &mut proj, &ev) {
            if subs.iter().any(|s| hev::matches(s, name, pane.as_deref())) {
                return Ok(typed("event", hev::stream_event(name, data)));
            }
        }
    }
}

// ---- cross-session plugin identity ------------------------------------------------------------

/// How long a cross-session ticket stays valid (it is used immediately by the shim).
const TICKET_TTL: Duration = Duration::from_secs(30);

/// `vibeke.invocation_ticket` (broker connections only): a single-use ticket that lets another
/// session verify this invocation's identity with this server.
fn invocation_ticket(server: &Server, caller: &Caller) -> Result<Value, WireError> {
    let Some(b) = &caller.broker else {
        return Err(wire::unknown_method("vibeke.invocation_ticket"));
    };
    if caller.sandboxed {
        return Err(sandbox_denied("vibeke.invocation_ticket"));
    }
    let bytes: [u8; 32] = rand::random();
    let ticket: String = bytes.iter().map(|x| format!("{x:02x}")).collect();
    let st = state(server);
    let mut t = st.tickets.lock().unwrap();
    let now = std::time::Instant::now();
    t.retain(|_, (_, exp)| *exp > now);
    t.insert(ticket.clone(), (b.clone(), now + TICKET_TTL));
    Ok(typed(
        "invocation_ticket",
        json!({"ticket": ticket, "session": server.opts.session}),
    ))
}

/// Redeem a ticket issued by this server: the issuing broker must still be open, its
/// invocation alive and its grant unchanged. Single use.
fn redeem_ticket(server: &Server, ticket: &str) -> Result<Value, String> {
    let entry = state(server).tickets.lock().unwrap().remove(ticket);
    let Some((path, exp)) = entry else {
        return Err("unknown or already used ticket".into());
    };
    if exp <= std::time::Instant::now() {
        return Err("ticket expired".into());
    }
    let b = brokers::live(server, &path).ok_or("the issuing invocation has ended")?;
    if !brokers::grant_ok(&b.plugin_id, &b.digest, &b.grant_id) {
        return Err(format!(
            "plugin {} is no longer trusted or enabled",
            b.plugin_id
        ));
    }
    Ok(json!({
        "plugin_id": b.plugin_id,
        "digest": b.digest,
        "grant_id": b.grant_id,
        "log_id": b.log_id,
        "session": server.opts.session,
        "sandboxed": b.isolation.as_deref() != Some("host"),
    }))
}

/// Verify a ticket with the session that issued it (over its native socket; in process when
/// that is this session).
async fn verify_ticket(server: &Server, session: &str, ticket: &str) -> Result<Value, String> {
    if !herdr::valid_session_name(session) {
        return Err(format!("invalid session name {session:?}"));
    }
    if session == server.opts.session {
        return redeem_ticket(server, ticket);
    }
    let sock = crate::paths::Paths::new(session).socket();
    let io = async {
        let s = UnixStream::connect(&sock).await?;
        let (rd, mut wr) = s.into_split();
        let req = json!({"jsonrpc": "2.0", "id": 1, "method": "compat.invocation.verify", "params": {"ticket": ticket}});
        wr.write_all(format!("{req}\n").as_bytes()).await?;
        let mut line = String::new();
        BufReader::new(rd).read_line(&mut line).await?;
        Ok::<_, std::io::Error>(line)
    };
    let line = tokio::time::timeout(Duration::from_secs(5), io)
        .await
        .map_err(|_| format!("session {session} did not answer"))?
        .map_err(|e| format!("session {session}: {e}"))?;
    let v: Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
    match v.get("result") {
        Some(r) if r.is_object() => Ok(r.clone()),
        _ => Err(v["error"]["message"]
            .as_str()
            .unwrap_or("ticket rejected")
            .to_string()),
    }
}

// ---- plugins: listing, invocation, logs -------------------------------------------------------

fn plugin_list() -> Vec<Value> {
    let dirs = plugin_dirs();
    let Ok(reg) = Registry::load(&dirs) else {
        return vec![];
    };
    let keys = views::keybindings(&reg);
    reg.plugins
        .values()
        .map(|e| {
            let (st, m) = registry::entry_status(e);
            let cfg = limits::settings(&e.id);
            let sandboxed = cfg.isolate == herdr::settings::Isolate::Sandbox;
            let sandbox_err = if sandboxed {
                vk_sandbox::plugin::probe().err()
            } else {
                None
            };
            json!({
                "origin": e.origin,
                "isolate": if sandboxed { "sandbox" } else { "host" },
                "sandbox_available": sandboxed.then_some(sandbox_err.is_none()),
                "sandbox_error": sandbox_err,
                "network": sandboxed.then_some(cfg.network),
                "max_concurrent": cfg.max_concurrent,
                "settings_warnings": cfg.warnings,
                "settings_error": cfg.error,
                "output_max_bytes": cfg.output_max_bytes,
                "keybindings": keys
                    .iter()
                    .filter(|k| k["plugin_id"] == e.id.as_str())
                    .cloned()
                    .collect::<Vec<_>>(),
                "plugin_id": e.id,
                "name": m.as_ref().and_then(|m| m.name.clone()),
                "version": m.as_ref().and_then(|m| m.version.clone()),
                "enabled": e.enabled,
                "status": st.as_str(),
                "trust": e.trust.as_ref().map(|g| g.mode.clone()),
                "managed": e.managed,
                "root": e.root,
                "source": e.origin.path,
                "config_dir": dirs.config_dir(&e.id),
                "state_dir": dirs.state_dir(&e.id),
                "actions": m.as_ref().map(|m| m.actions_on(herdr::current_platform()).len()),
                "events": m.as_ref().map(|m| m.events.iter().map(|e| e.on.clone()).collect::<Vec<_>>()),
            })
        })
        .collect()
}

/// Herdr's `PluginSourceInfo` of a registration: `github` with owner, repo, subdirectory,
/// ref, commit and managed checkout for repository installs; `local` otherwise.
fn herdr_source(e: &Entry) -> Value {
    if e.origin.kind != "git" {
        return json!({"kind": "local"});
    }
    let mut v = Map::new();
    v.insert("kind".into(), json!("github"));
    if let Some(r) = &e.origin.repo {
        let mut parts = r.splitn(3, '/');
        for k in ["owner", "repo", "subdir"] {
            if let Some(x) = parts.next().filter(|x| !x.is_empty()) {
                v.insert(k.into(), json!(x));
            }
        }
    }
    if let Some(r) = &e.origin.requested_ref {
        v.insert("requested_ref".into(), json!(r));
    }
    if let Some(c) = &e.origin.commit {
        v.insert("resolved_commit".into(), json!(c));
    }
    if e.managed {
        v.insert("managed_path".into(), json!(e.root.display().to_string()));
    }
    if e.installed_at_ms > 0 {
        v.insert("installed_unix_ms".into(), json!(e.installed_at_ms));
    }
    Value::Object(v)
}

/// Herdr's `PluginManifestAction` (`id`, `title`, `description?`, declared `contexts`,
/// `platforms?`, `command`).
fn herdr_manifest_action(a: &herdr::manifest::Action) -> Value {
    let mut v = Map::new();
    v.insert("id".into(), json!(a.id));
    v.insert("title".into(), json!(a.title));
    if let Some(d) = &a.description {
        v.insert("description".into(), json!(d));
    }
    if a.contexts_declared && !a.contexts.is_empty() {
        v.insert("contexts".into(), json!(a.contexts));
    }
    if let Some(p) = &a.platforms {
        v.insert("platforms".into(), json!(p));
    }
    v.insert("command".into(), json!(a.command));
    Value::Object(v)
}

/// Herdr's `InstalledPluginInfo` of a registration: manifest fields in Herdr's order (actions
/// and panes by id, events by name then command), empty lists left out, `source` as
/// `PluginSourceInfo`, `warnings` with `manifest unavailable: …` for a manifest that no longer
/// loads.
fn herdr_installed_info(e: &Entry) -> Value {
    let manifest_path = e.root.join(herdr::MANIFEST_FILE);
    let manifest_path = std::fs::canonicalize(&manifest_path).unwrap_or(manifest_path);
    let (_, m) = registry::entry_status(e);
    let mut v = Map::new();
    v.insert("plugin_id".into(), json!(e.id));
    v.insert(
        "name".into(),
        json!(
            m.as_ref()
                .and_then(|m| m.name.clone())
                .unwrap_or_else(|| e.id.clone())
        ),
    );
    v.insert(
        "version".into(),
        json!(
            m.as_ref()
                .and_then(|m| m.version.clone())
                .unwrap_or_default()
        ),
    );
    v.insert(
        "min_herdr_version".into(),
        json!(
            m.as_ref()
                .and_then(|m| m.min_herdr_version.clone())
                .unwrap_or_default()
        ),
    );
    if let Some(d) = m.as_ref().and_then(|m| m.description.clone()) {
        v.insert("description".into(), json!(d));
    }
    v.insert(
        "manifest_path".into(),
        json!(manifest_path.display().to_string()),
    );
    v.insert("plugin_root".into(), json!(e.root.display().to_string()));
    v.insert("enabled".into(), json!(e.enabled));
    let mut warnings = Vec::new();
    match &m {
        Some(m) => {
            if let Some(p) = &m.platforms {
                v.insert("platforms".into(), json!(p));
            }
            let mut put = |k: &str, x: Value| {
                if x.as_array().is_some_and(|a| !a.is_empty()) {
                    v.insert(k.into(), x);
                }
            };
            put("build", json!(m.build));
            put("startup", json!(m.startup));
            let mut actions: Vec<&herdr::manifest::Action> = m.actions.iter().collect();
            actions.sort_by(|a, b| a.id.cmp(&b.id));
            put(
                "actions",
                json!(
                    actions
                        .iter()
                        .map(|a| herdr_manifest_action(a))
                        .collect::<Vec<_>>()
                ),
            );
            let mut events: Vec<&herdr::manifest::EventHook> = m.events.iter().collect();
            events.sort_by(|a, b| a.on.cmp(&b.on).then_with(|| a.command.cmp(&b.command)));
            put(
                "events",
                json!(
                    events
                        .iter()
                        .map(|h| {
                            let mut x = json!({"on": h.on, "command": h.command});
                            if let Some(p) = &h.platforms {
                                x["platforms"] = json!(p);
                            }
                            x
                        })
                        .collect::<Vec<_>>()
                ),
            );
            let mut panes: Vec<&herdr::manifest::PaneDecl> = m.panes.iter().collect();
            panes.sort_by(|a, b| a.id.cmp(&b.id));
            put("panes", json!(panes));
            put("link_handlers", json!(m.link_handlers));
            warnings.extend(m.warnings.iter().cloned());
            if m.platforms.is_none() {
                warnings.push(
                    "manifest does not declare platforms; platform support unknown".to_string(),
                );
            }
        }
        None => {
            let why = registry::read_manifest(&e.root)
                .err()
                .map(|e| e.to_string())
                .unwrap_or_else(|| "manifest does not load".to_string());
            warnings.push(format!("manifest unavailable: {why}"));
        }
    }
    v.insert("source".into(), herdr_source(e));
    if !warnings.is_empty() {
        v.insert("warnings".into(), json!(warnings));
    }
    Value::Object(v)
}

/// `plugin.list` and the registry results for the Herdr surface: Herdr's `InstalledPluginInfo`
/// over Vibeke's own record, whose extra fields (`status`, `trust`, `managed`, `origin`,
/// `config_dir`, …) Herdr clients ignore.
pub fn herdr_plugin_list(filter: Option<&str>) -> Vec<Value> {
    let dirs = plugin_dirs();
    let Ok(reg) = Registry::load(&dirs) else {
        return vec![];
    };
    let mut extras: HashMap<String, Value> = plugin_list()
        .into_iter()
        .filter_map(|p| Some((p["plugin_id"].as_str()?.to_string(), p)))
        .collect();
    reg.plugins
        .values()
        .filter(|e| filter.is_none_or(|f| f == e.id))
        .map(|e| {
            let mut v = extras.remove(&e.id).unwrap_or_else(|| json!({}));
            if let (Value::Object(v), Value::Object(h)) = (&mut v, herdr_installed_info(e)) {
                v.extend(h);
            }
            v
        })
        .collect()
}

/// Herdr `plugin.action.list`: every action of every plugin whose manifest loads, sorted by
/// qualified id, as `PluginActionInfo` (`command` is the manifest argv, `platforms` the
/// action's or else the plugin's). Vibeke adds `qualified_id`, `available` and `status`.
fn herdr_action_list(plugin: Option<&str>) -> Result<Vec<Value>, WireError> {
    let reg = Registry::load(&plugin_dirs())
        .map_err(|e| WireError::new("plugin_registry_load_failed", e.to_string()))?;
    let pf = herdr::current_platform();
    let mut out = Vec::new();
    for e in reg
        .plugins
        .values()
        .filter(|e| plugin.is_none_or(|p| p == e.id))
    {
        let (st, m) = registry::entry_status(e);
        let Some(m) = m else { continue };
        for a in &m.actions {
            let mut v = herdr_manifest_action(a);
            let platforms = a.platforms.as_ref().or(m.platforms.as_ref());
            if let Value::Object(o) = &mut v {
                let id = o.remove("id").unwrap_or_default();
                o.insert("plugin_id".into(), json!(e.id));
                o.insert("action_id".into(), id);
                o.remove("platforms");
                if let Some(p) = platforms {
                    o.insert("platforms".into(), json!(p));
                }
                o.insert("qualified_id".into(), json!(format!("{}.{}", e.id, a.id)));
                o.insert(
                    "available".into(),
                    json!(st == Status::Active && m.entry_on(a.platforms.as_ref(), pf)),
                );
                o.insert("status".into(), json!(st.as_str()));
            }
            out.push(v);
        }
    }
    out.sort_by(|a, b| {
        a["qualified_id"]
            .as_str()
            .unwrap_or("")
            .cmp(b["qualified_id"].as_str().unwrap_or(""))
    });
    Ok(out)
}

/// Actions of active plugins (inactive plugins are listed with `available: false` so the
/// palette can explain why).
fn action_list(plugin: Option<&str>) -> Result<Vec<Value>, WireError> {
    let reg = Registry::load(&plugin_dirs())
        .map_err(|e| WireError::new("internal_error", e.to_string()))?;
    if let Some(id) = plugin
        && reg.get(id).is_err()
    {
        return Err(WireError::new(
            "plugin_not_found",
            "plugin not found".to_string(),
        ));
    }
    let pf = herdr::current_platform();
    let mut out = Vec::new();
    for e in reg
        .plugins
        .values()
        .filter(|e| plugin.is_none_or(|p| p == e.id))
    {
        let (st, m) = registry::entry_status(e);
        let Some(m) = m else { continue };
        for a in m.actions_on(pf) {
            out.push(json!({
                "plugin_id": e.id,
                "action_id": a.id,
                "qualified_id": format!("{}.{}", e.id, a.id),
                "title": a.title,
                "description": a.description,
                "contexts": a.contexts,
                "available": st == Status::Active,
                "status": st.as_str(),
            }));
        }
    }
    Ok(out)
}

/// Link handlers of every registered plugin in matching order (registry order, then manifest
/// order; the baseline order is unverified). Inactive plugins are listed with
/// `available: false` so a client can explain why a handler can't run.
fn link_handler_list() -> Result<Vec<Value>, WireError> {
    let reg = Registry::load(&plugin_dirs())
        .map_err(|e| WireError::new("internal_error", e.to_string()))?;
    let pf = herdr::current_platform();
    let mut out = Vec::new();
    for e in reg.plugins.values() {
        let (st, m) = registry::entry_status(e);
        let Some(m) = m else { continue };
        for l in m
            .link_handlers
            .iter()
            .filter(|l| m.entry_on(l.platforms.as_ref(), pf))
        {
            let action = m.resolve_action(&l.action).map(|a| a.id.clone());
            out.push(json!({
                "plugin_id": e.id,
                "handler_id": l.id,
                "title": l.title,
                "pattern": l.pattern,
                "action_id": action,
                "available": st == Status::Active && action.is_some(),
                "status": st.as_str(),
            }));
        }
    }
    Ok(out)
}

/// Run a plugin's link handler for `url`: the handler must exist and match; its action gets
/// `HERDR_PLUGIN_CLICKED_URL` and `HERDR_PLUGIN_LINK_HANDLER_ID` (07 §7.7).
fn open_link(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> Result<Value, WireError> {
    let s = |k: &str| p.get(k).and_then(Value::as_str);
    let (Some(plugin), Some(handler), Some(url)) = (s("plugin"), s("handler"), s("url")) else {
        return Err(WireError::new(
            "invalid_params",
            "plugin, handler and url are required",
        ));
    };
    let reg = Registry::load(&plugin_dirs())
        .map_err(|e| WireError::new("internal_error", e.to_string()))?;
    let entry = reg
        .get(plugin)
        .map_err(|_| WireError::new("plugin_not_found", "plugin not found".to_string()))?;
    let (_, m) = registry::entry_status(entry);
    let m = m.ok_or_else(|| WireError::new("invalid_manifest", "manifest missing"))?;
    let pf = herdr::current_platform();
    let l = m
        .link_handlers
        .iter()
        .find(|l| l.id == handler && m.entry_on(l.platforms.as_ref(), pf))
        .ok_or_else(|| {
            WireError::new(
                "link_handler_not_found",
                format!("{plugin} has no link handler `{handler}`"),
            )
        })?;
    let re = regex::Regex::new(&l.pattern)
        .map_err(|e| WireError::new("invalid_manifest", e.to_string()))?;
    if !re.is_match(url) {
        return Err(WireError::new(
            "invalid_params",
            format!("{url} does not match link handler `{handler}`"),
        ));
    }
    let action = m
        .resolve_action(&l.action)
        .map(|a| a.id.clone())
        .ok_or_else(|| WireError::new("plugin_action_not_found", l.action.clone()))?;
    // Herdr: the clicked pane's context, source and correlation `link_click`.
    let sn = snap(server);
    let mut ictx = InvokeContext::from_params(server, &sn, p, "link_click", "link_click");
    ictx.clicked_url = Some(url.to_string());
    ictx.link_handler_id = Some(l.id.clone());
    invoke_action(
        server,
        ctx.pane_scope.as_deref(),
        plugin,
        &action,
        ictx,
        "link_handler",
    )
}

// ---- pane links (`pane.link.resolve`, `pane.link.activate`) -----------------------------------

/// The cells of one terminal row at viewport row `vrow`, one grapheme each (combining marks
/// join the cell before them; a wide character covers two columns).
fn row_cells(row: &vk_proto::render::Row, vrow: i64, out: &mut Vec<herdr::links::Cell>) {
    use unicode_width::UnicodeWidthChar;
    let mut col: u16 = 0;
    for span in &row.spans {
        let start = col;
        for ch in span.text.chars() {
            let w = ch.width().unwrap_or(0) as u16;
            if w == 0 {
                if let Some(last) = out.last_mut().filter(|c| c.row == vrow) {
                    last.text.push(ch);
                }
                continue;
            }
            out.push(herdr::links::Cell {
                row: vrow,
                col,
                end_col: col.saturating_add(w - 1),
                text: ch.to_string(),
            });
            col = col.saturating_add(w);
        }
        col = start.saturating_add(span.cols).max(col);
    }
}

/// What lies under a viewport cell: an OSC 8 target, or the plain-text token (its cells, the
/// token's cell range and the clicked cell).
enum LinkHit {
    Uri(String),
    Text {
        cells: Vec<herdr::links::Cell>,
        token: (usize, usize),
        clicked: usize,
    },
}

/// Herdr's `viewport_link_selection` over Vibeke's engine: the viewport is `offset` rows above
/// the bottom; a token continues across soft-wrapped rows (also outside the viewport).
fn link_hit(engine: &vk_term::Engine, offset: u64, col: u16, vrow: u16) -> Option<LinkHit> {
    let (rows, cols) = (engine.rows(), engine.cols());
    if col >= cols || vrow >= rows {
        return None;
    }
    let hist = engine.history_len() as i64;
    let total = hist + i64::from(rows);
    let top = hist - offset.min(hist as u64) as i64;
    let fetch = |v: i64| -> Option<vk_proto::render::Row> {
        let abs = top + v;
        if abs < 0 || abs >= total {
            None
        } else if abs < hist {
            engine.history_row(abs as usize)
        } else {
            Some(engine.row((abs - hist) as u16))
        }
    };
    let here = fetch(i64::from(vrow))?;
    if let Some(l) = here.link_at(col) {
        return Some(LinkHit::Uri(l.uri.clone()));
    }
    // The logical line: rows joined by soft wraps (bounded like Herdr's word selection).
    let v = i64::from(vrow);
    let mut first = v;
    while v - first < 64 && fetch(first - 1).is_some_and(|r| r.wrapped) {
        first -= 1;
    }
    let mut last = v;
    while last - v < 64 && fetch(last).is_some_and(|r| r.wrapped) && fetch(last + 1).is_some() {
        last += 1;
    }
    let mut cells = Vec::new();
    let mut clicked = None;
    for r in first..=last {
        let row = if r == v { here.clone() } else { fetch(r)? };
        let from = cells.len();
        row_cells(&row, r, &mut cells);
        if r == v {
            clicked = cells[from..]
                .iter()
                .position(|c| c.col <= col && col <= c.end_col)
                .map(|i| from + i);
        }
    }
    let clicked = clicked?;
    let token = herdr::links::token(&cells, clicked)?;
    Some(LinkHit::Text {
        cells,
        token,
        clicked,
    })
}

/// The first link handler whose pattern matches `url` (plugins in id order, active ones only;
/// the handler and its action must run on this platform): `(plugin, action, handler)`.
fn find_link_handler(url: &str) -> Option<(String, String, String)> {
    let reg = Registry::load(&plugin_dirs()).ok()?;
    let pf = herdr::current_platform();
    for e in reg.plugins.values() {
        let (st, m) = registry::entry_status(e);
        let (Status::Active, Some(m)) = (st, m) else {
            continue;
        };
        for l in &m.link_handlers {
            if !m.entry_on(l.platforms.as_ref(), pf) {
                continue;
            }
            let Some(a) = m.resolve_action(&l.action) else {
                continue;
            };
            if !m.entry_on(a.platforms.as_ref(), pf) {
                continue;
            }
            let Ok(re) = regex::Regex::new(&l.pattern) else {
                continue;
            };
            if re.is_match(url) {
                return Some((e.id.clone(), a.id.clone(), l.id.clone()));
            }
        }
    }
    None
}

/// Herdr `pane.link.resolve` / `pane.link.activate` `{pane_id, viewport_row, col,
/// content_revision?, offset_from_bottom?}` (`read_checked_pane_link`): the pane must be
/// visible (`stale_target`), the viewport offset and content revision must still match
/// (`stale_content`). Resolve answers `pane_link_resolved {regions}` (the plain-text URL's
/// cells; an OSC 8 link has none, as in Herdr); activate finds the URL (an OSC 8 target, or a
/// plain `http(s)` URL) and runs the first matching plugin link handler with the clicked
/// pane's context, `clicked_url` and `link_handler_id`, answering
/// `pane_link_activated {url?, handled}`.
async fn pane_link(
    server: &Arc<Server>,
    caller: &Caller,
    sn: &Snap,
    method: &str,
    p: &Value,
) -> Result<Value, WireError> {
    let field = |k: &str| -> Result<u16, WireError> {
        match p.get(k) {
            None | Some(Value::Null) => Err(wire::invalid_request(format!("missing field `{k}`"))),
            Some(v) => v
                .as_u64()
                .and_then(|n| u16::try_from(n).ok())
                .ok_or_else(|| wire::invalid_request(format!("invalid value for `{k}`"))),
        }
    };
    let pid = sp(p, "pane_id").ok_or_else(|| wire::invalid_request("missing field `pane_id`"))?;
    let (vrow, col) = (field("viewport_row")?, field("col")?);
    let op = if method == "pane.link.resolve" {
        "resolution"
    } else {
        "activation"
    };
    let x = sn
        .pane(pid)
        .cloned()
        .ok_or_else(|| WireError::new("pane_not_found", "pane not found"))?;
    let visible = sn
        .tab(&x.tab)
        .is_some_and(|t| sn.tab_is_active(t) && t.zoomed_pane.as_deref().is_none_or(|z| z == x.id));
    if !visible {
        return Err(WireError::new("stale_target", "pane is no longer visible"));
    }
    let rt = server
        .pane_rt(&x.id)
        .ok_or_else(|| WireError::new("pane_not_found", "pane runtime not found"))?;
    let offset = u64::from(
        sn.meta
            .scroll
            .get(&x.id)
            .or_else(|| sn.meta.scroll.get(&x.handle))
            .copied()
            .unwrap_or(0),
    );
    if p.get("offset_from_bottom")
        .and_then(Value::as_u64)
        .is_some_and(|o| o != offset)
    {
        return Err(WireError::new(
            "stale_content",
            format!("pane viewport changed before link {op}"),
        ));
    }
    let (hit, rows) = {
        let sc = rt.screen.lock().unwrap();
        if p.get("content_revision")
            .and_then(Value::as_u64)
            .is_some_and(|r| r != sc.rev)
        {
            return Err(WireError::new(
                "stale_content",
                format!("pane content changed before link {op}"),
            ));
        }
        (link_hit(&sc.engine, offset, col, vrow), sc.engine.rows())
    };
    if method == "pane.link.resolve" {
        let regions: Vec<Value> = match &hit {
            Some(LinkHit::Text {
                cells,
                token,
                clicked,
            }) => {
                let (text, b) = herdr::links::token_text(cells, *token, *clicked);
                herdr::links::url_byte_range(&text, b)
                    .map(|r| herdr::links::regions(cells, *token, &r, rows))
                    .unwrap_or_default()
                    .into_iter()
                    .map(|r| json!({"row": r.row, "start_col": r.start_col, "end_col": r.end_col}))
                    .collect()
            }
            _ => Vec::new(),
        };
        return Ok(typed("pane_link_resolved", json!({"regions": regions})));
    }
    let url = match hit {
        Some(LinkHit::Uri(u)) => Some(u),
        Some(LinkHit::Text {
            cells,
            token,
            clicked,
        }) => {
            let (text, b) = herdr::links::token_text(&cells, token, clicked);
            herdr::links::url_at_byte(&text, b).map(str::to_string)
        }
        None => None,
    };
    let mut handled = false;
    if let Some(u) = &url
        && let Some((plugin, action, handler)) = find_link_handler(u)
        && let Some(mut ictx) = sn.ctx_for_pane(server, &x, "link_click")
    {
        ictx.invocation_source = Some("link_click".into());
        ictx.clicked_url = Some(u.clone());
        ictx.link_handler_id = Some(handler);
        match invoke_action(
            server,
            caller.ctx.pane_scope.as_deref(),
            &plugin,
            &action,
            ictx,
            "link_handler",
        ) {
            Ok(_) => handled = true,
            Err(e) => tracing::warn!(url = %u, error = %e, "plugin link handler not invoked"),
        }
    }
    let mut v = json!({"handled": handled});
    if let Some(u) = url {
        v["url"] = json!(u);
    }
    Ok(typed("pane_link_activated", v))
}

fn split_qualified(q: &str) -> Result<(String, String), WireError> {
    // Plugin ids contain dots; the longest registered prefix wins.
    let reg = Registry::load(&plugin_dirs())
        .map_err(|e| WireError::new("internal_error", e.to_string()))?;
    reg.plugins
        .keys()
        .filter(|id| q.starts_with(&format!("{id}.")))
        .max_by_key(|id| id.len())
        .map(|id| (id.clone(), q[id.len() + 1..].to_string()))
        .ok_or_else(|| {
            WireError::new(
                "plugin_not_found",
                format!("no plugin matches action `{q}`"),
            )
        })
}

/// Herdr's `PluginInvocationContext` (schema `PluginInvocationContext`, Herdr
/// `src/app/api/plugins/context.rs`): `HERDR_PLUGIN_CONTEXT_JSON` and the `context` of
/// `plugin_action_invoked`. Absent fields are left out of the JSON, as in Herdr.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InvokeContext {
    pub workspace_id: Option<String>,
    pub workspace_label: Option<String>,
    pub workspace_cwd: Option<String>,
    /// `WorkspaceWorktreeInfo`.
    pub worktree: Option<Value>,
    pub tab_id: Option<String>,
    pub tab_label: Option<String>,
    pub focused_pane_id: Option<String>,
    pub focused_pane_cwd: Option<String>,
    pub focused_pane_agent: Option<String>,
    /// `AgentStatus`.
    pub focused_pane_status: Option<String>,
    pub selected_text: Option<String>,
    pub invocation_source: Option<String>,
    pub correlation_id: Option<String>,
    /// Link handler invocations (07 §7.7): the activated URL and the handler's id.
    pub clicked_url: Option<String>,
    pub link_handler_id: Option<String>,
}

impl InvokeContext {
    /// Herdr `empty_plugin_context`.
    fn empty(correlation: &str) -> Self {
        InvokeContext {
            invocation_source: Some("api".into()),
            correlation_id: Some(correlation.into()),
            ..Default::default()
        }
    }

    /// The JSON object, in Herdr's field order.
    pub fn to_json(&self) -> Map<String, Value> {
        let mut m = Map::new();
        let mut put = |k: &str, v: &Option<String>| {
            if let Some(v) = v {
                m.insert(k.into(), json!(v));
            }
        };
        put("workspace_id", &self.workspace_id);
        put("workspace_label", &self.workspace_label);
        put("workspace_cwd", &self.workspace_cwd);
        if let Some(w) = &self.worktree {
            m.insert("worktree".into(), w.clone());
        }
        let mut put = |k: &str, v: &Option<String>| {
            if let Some(v) = v {
                m.insert(k.into(), json!(v));
            }
        };
        put("tab_id", &self.tab_id);
        put("tab_label", &self.tab_label);
        put("focused_pane_id", &self.focused_pane_id);
        put("focused_pane_cwd", &self.focused_pane_cwd);
        put("focused_pane_agent", &self.focused_pane_agent);
        put("focused_pane_status", &self.focused_pane_status);
        put("selected_text", &self.selected_text);
        put("invocation_source", &self.invocation_source);
        put("correlation_id", &self.correlation_id);
        put("clicked_url", &self.clicked_url);
        put("link_handler_id", &self.link_handler_id);
        m
    }

    /// Herdr `merge_plugin_context`: every field the caller provided replaces the base's.
    fn merge(mut self, provided: &Value) -> Self {
        let over = |dst: &mut Option<String>, k: &str| {
            if let Some(v) = provided.get(k).and_then(Value::as_str) {
                *dst = Some(v.to_string());
            }
        };
        over(&mut self.workspace_id, "workspace_id");
        over(&mut self.workspace_label, "workspace_label");
        over(&mut self.workspace_cwd, "workspace_cwd");
        if let Some(w) = provided.get("worktree").filter(|w| w.is_object()) {
            self.worktree = Some(w.clone());
        }
        over(&mut self.tab_id, "tab_id");
        over(&mut self.tab_label, "tab_label");
        over(&mut self.focused_pane_id, "focused_pane_id");
        over(&mut self.focused_pane_cwd, "focused_pane_cwd");
        over(&mut self.focused_pane_agent, "focused_pane_agent");
        over(&mut self.focused_pane_status, "focused_pane_status");
        over(&mut self.selected_text, "selected_text");
        over(&mut self.invocation_source, "invocation_source");
        over(&mut self.correlation_id, "correlation_id");
        over(&mut self.clicked_url, "clicked_url");
        over(&mut self.link_handler_id, "link_handler_id");
        self
    }

    /// The context of an action run through the API: Herdr's current context (the active
    /// workspace and its focused pane, kept server-side without a client) merged with the
    /// request's `context` field by field. Vibeke's own callers (TUI, `vibeke plugin action
    /// run`, link handlers) may name the pane, tab or workspace as top-level `pane`, `tab` or
    /// `workspace`: the base context is then built for that object instead.
    fn from_params(server: &Server, sn: &Snap, p: &Value, source: &str, correlation: &str) -> Self {
        let base = sp(p, "pane")
            .and_then(|x| sn.pane(x))
            .and_then(|x| sn.ctx_for_pane(server, x, correlation))
            .or_else(|| {
                sp(p, "tab")
                    .and_then(|x| sn.tab(x))
                    .and_then(|x| sn.ctx_for_tab(server, x, correlation))
            })
            .or_else(|| {
                sp(p, "workspace")
                    .and_then(|x| sn.ws(x))
                    .map(|x| sn.ctx_for_ws(server, x, correlation))
            })
            .unwrap_or_else(|| sn.current_ctx(server, correlation));
        let mut ctx = InvokeContext {
            invocation_source: Some(source.to_string()),
            ..base
        };
        if let Some(c) = p.get("context").filter(|c| c.is_object()) {
            ctx = ctx.merge(c);
        }
        ctx
    }
}

fn now_ms() -> i64 {
    vk_store::now_ms()
}

/// Run an action of a trusted plugin asynchronously; returns the running log record.
/// Pane-scoped callers are refused: an agent cannot gain legacy authority by invoking a plugin
/// (09 §6 no-escalation).
pub fn invoke_action(
    server: &Arc<Server>,
    pane_scope: Option<&str>,
    plugin: &str,
    action: &str,
    ctx: InvokeContext,
    source: &str,
) -> Result<Value, WireError> {
    if pane_scope.is_some() {
        return Err(WireError::new(
            "permission_denied",
            "legacy plugin actions cannot be invoked from a pane; ask the user to run them",
        ));
    }
    let dirs = plugin_dirs();
    let reg = Registry::load(&dirs).map_err(|e| WireError::new("internal_error", e.to_string()))?;
    let entry = reg
        .get(plugin)
        .map_err(|_| WireError::new("plugin_not_found", "plugin not found".to_string()))?
        .clone();
    let (st, m) = registry::entry_status(&entry);
    if st != Status::Active {
        return Err(WireError::new(
            "permission_denied",
            format!(
                "plugin {plugin} is {}{}",
                st.as_str(),
                match st {
                    Status::Untrusted | Status::StaleTrust =>
                        format!("; review it with `vibeke plugin trust {plugin} --legacy`"),
                    Status::Disabled => format!("; `vibeke plugin enable {plugin}`"),
                    _ => String::new(),
                }
            ),
        ));
    }
    let m = m.expect("active plugins have a manifest");
    let a = m
        .action_for(action, herdr::current_platform())
        .ok_or_else(|| {
            WireError::new(
                "plugin_action_not_found",
                format!(
                    "{plugin} has no action `{action}` on {}",
                    herdr::current_platform()
                ),
            )
        })?
        .clone();
    // At most `max_concurrent` running invocations per plugin: a further action run is refused
    // with `busy` (event hooks queue instead).
    let slot = limits::try_slot(server, &entry.id).ok_or_else(|| limits::busy(&entry.id))?;
    Ok(spawn_invocation(
        server,
        &entry,
        &m,
        &a.command,
        Spawn {
            source,
            action: Some(a.id.clone()),
            event: None,
            entrypoint: Some(a.id.clone()),
            ctx,
            long_lived: false,
        },
        Some(slot),
    ))
}

struct Spawn<'a> {
    source: &'a str,
    action: Option<String>,
    event: Option<(String, Value)>,
    entrypoint: Option<String>,
    ctx: InvokeContext,
    /// The manifest declares this entrypoint long-running (`[[startup]]`): its broker stays
    /// while the invocation's process group is alive.
    long_lived: bool,
}

fn logs_path(server: &Server) -> PathBuf {
    compat_root(server).join("logs.json")
}

/// Persist the log records (so a restarted server still lists them, 07 §7.7).
fn persist_logs(server: &Server) {
    let st = state(server);
    // Keep the file small: the persisted copy holds the last 4 KiB of each stream.
    let trim = |v: &Value| -> Value {
        let mut v = v.clone();
        for k in ["stdout", "stderr"] {
            if let Some(t) = v.get(k).and_then(Value::as_str)
                && t.len() > 4096
            {
                let mut cut = t.len() - 4096;
                while !t.is_char_boundary(cut) {
                    cut += 1;
                }
                v[k] = json!(t[cut..].to_string());
            }
        }
        v
    };
    let bytes = {
        let logs = st.logs.lock().unwrap();
        serde_json::to_vec(&logs.iter().map(trim).collect::<Vec<_>>()).unwrap_or_default()
    };
    if private_dir(&compat_root(server)).is_err() {
        return;
    }
    let path = logs_path(server);
    let tmp = path.with_extension("json.tmp");
    use std::os::unix::fs::OpenOptionsExt;
    let ok = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| std::io::Write::write_all(&mut f, &bytes));
    if ok.is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Load persisted log records at server start. Records still `running` whose invocation is not
/// re-attached by broker recovery are finalized with an unknown exit status.
fn load_logs(server: &Server) {
    let Some(list) = std::fs::read(logs_path(server))
        .ok()
        .and_then(|b| serde_json::from_slice::<Vec<Value>>(&b).ok())
    else {
        return;
    };
    let st = state(server);
    let mut logs = st.logs.lock().unwrap();
    for mut l in list {
        if l["status"] == "running" {
            let pid = l["pid"].as_u64().map(|p| p as i32);
            if !pid.is_some_and(brokers::pid_alive) {
                l["status"] = json!("failed");
                l["error"] = json!("the server restarted; exit status unknown");
                if l["finished_at"].is_null() {
                    l["finished_at"] = json!(now_ms());
                    l["finished_unix_ms"] = l["finished_at"].clone();
                }
            }
        }
        // Records written before the baseline log fields.
        if l["status"] == "completed" {
            l["status"] = json!("succeeded");
        }
        if l.get("started_unix_ms").is_none() {
            l["started_unix_ms"] = l["started_at"].clone();
        }
        logs.push_back(l);
    }
    while logs.len() > MAX_LOGS {
        logs.pop_front();
    }
    let n = logs.len() as u64;
    drop(logs);
    st.next.fetch_max(n, Ordering::Relaxed);
}

fn update_log(server: &Server, id: &str, f: impl FnOnce(&mut Map<String, Value>)) {
    let st = state(server);
    let mut logs = st.logs.lock().unwrap();
    if let Some(Value::Object(o)) = logs.iter_mut().find(|l| l["log_id"] == id) {
        f(o);
    }
}

fn push_log(server: &Server, rec: Value) {
    {
        let st = state(server);
        let plugin = rec["plugin_id"].as_str().map(str::to_string);
        let cap = plugin
            .as_deref()
            .map(|p| limits::settings(p).log_max_records);
        let mut logs = st.logs.lock().unwrap();
        logs.push_back(rec);
        while logs.len() > MAX_LOGS {
            logs.pop_front();
        }
        // The per-plugin ring: one chatty plugin cannot push everyone else's records out.
        if let (Some(p), Some(cap)) = (plugin, cap) {
            limits::trim_records(&mut logs, &p, cap);
        }
    }
    persist_logs(server);
}

fn logs(server: &Server, plugin: Option<&str>, limit: Option<u64>) -> Vec<Value> {
    let st = state(server);
    let logs = st.logs.lock().unwrap();
    let mut v: Vec<Value> = logs
        .iter()
        .filter(|l| plugin.is_none_or(|p| l["plugin_id"] == p))
        .cloned()
        .collect();
    if let Some(n) = limit {
        let n = n as usize;
        if v.len() > n {
            v.drain(..v.len() - n);
        }
    }
    v
}

/// Keep the last `caps.bytes` bytes and `caps.lines` lines of a stream; returns the number of
/// bytes dropped by this call (for the truncation marker).
fn tail_keep(buf: &mut Vec<u8>, caps: limits::LogCaps) -> u64 {
    let mut dropped = 0u64;
    if buf.len() > caps.bytes {
        let cut = buf.len() - caps.bytes;
        buf.drain(..cut);
        dropped += cut as u64;
    }
    let lines = buf.iter().filter(|b| **b == b'\n').count();
    if lines > caps.lines {
        // Drop leading lines until `caps.lines` complete lines remain.
        let mut skip = lines - caps.lines;
        let mut cut = 0;
        for (i, b) in buf.iter().enumerate() {
            if *b == b'\n' {
                skip -= 1;
                if skip == 0 {
                    cut = i + 1;
                    break;
                }
            }
        }
        buf.drain(..cut);
        dropped += cut as u64;
    }
    dropped
}

/// The text of a stream tail as stored in a log record: lossy UTF-8 with credentials redacted
/// (09 §6 "Logs and audit records redact credentials"). Output that fell off the front of the
/// ring is announced with a marker line.
fn redacted(buf: &[u8], dropped: u64) -> String {
    let text = String::from_utf8_lossy(buf);
    let body = vk_redact::redact(&text);
    if dropped == 0 {
        body.into_owned()
    } else {
        format!("{}{body}", limits::truncation_marker(dropped))
    }
}

/// Tails an invocation's stdout/stderr files into its log record.
struct Tail {
    /// `(file, read offset, kept tail, bytes dropped from the front)` for stdout and stderr.
    files: [(Option<PathBuf>, u64, Vec<u8>, u64); 2],
    caps: limits::LogCaps,
}

impl Tail {
    fn new(out: Option<PathBuf>, err: Option<PathBuf>, caps: limits::LogCaps) -> Self {
        Tail {
            files: [(out, 0, Vec::new(), 0), (err, 0, Vec::new(), 0)],
            caps,
        }
    }

    /// Read what was appended since the last poll; update the record when anything changed.
    /// A burst larger than the byte cap is skipped over without being read into memory.
    fn poll(&mut self, server: &Server, log_id: &str) {
        use std::io::{Read, Seek, SeekFrom};
        let mut changed = false;
        let caps = self.caps;
        for (path, off, buf, dropped) in self.files.iter_mut() {
            let Some(p) = path else { continue };
            let Ok(mut f) = std::fs::File::open(&*p) else {
                continue;
            };
            let len = f.metadata().map(|m| m.len()).unwrap_or(0);
            if len > *off && len - *off > caps.bytes as u64 {
                let skip = len - *off - caps.bytes as u64;
                *dropped += skip + buf.len() as u64;
                buf.clear();
                *off += skip;
            }
            if f.seek(SeekFrom::Start(*off)).is_err() {
                continue;
            }
            let mut chunk = Vec::new();
            if f.take(caps.bytes as u64 + 1)
                .read_to_end(&mut chunk)
                .is_ok()
                && !chunk.is_empty()
            {
                *off += chunk.len() as u64;
                buf.extend_from_slice(&chunk);
                *dropped += tail_keep(buf, caps);
                changed = true;
            }
        }
        if changed {
            let (o, e) = (
                redacted(&self.files[0].2, self.files[0].3),
                redacted(&self.files[1].2, self.files[1].3),
            );
            let (od, ed) = (self.files[0].3, self.files[1].3);
            update_log(server, log_id, |l| {
                l.insert("stdout".into(), json!(o));
                l.insert("stderr".into(), json!(e));
                l.insert("stdout_truncated_bytes".into(), json!(od));
                l.insert("stderr_truncated_bytes".into(), json!(ed));
            });
        }
    }

    /// The output files are transient (they may hold unredacted output): removed once read.
    fn remove(&self) {
        for (p, ..) in &self.files {
            if let Some(p) = p {
                let _ = std::fs::remove_file(p);
            }
        }
    }
}

/// Finish a log record and audit it.
fn finish_log(server: &Server, log_id: &str, ok: bool, code: Option<i32>, note: Option<&str>) {
    let mut meta = (String::new(), Value::Null, Value::Null);
    update_log(server, log_id, |l| {
        let now = now_ms();
        l.insert(
            "status".into(),
            json!(if ok { "succeeded" } else { "failed" }),
        );
        l.insert("exit_code".into(), json!(code));
        l.insert("finished_at".into(), json!(now));
        l.insert("finished_unix_ms".into(), json!(now));
        if let Some(n) = note {
            l.insert("error".into(), json!(n));
        }
        meta = (
            l.get("plugin_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            l.get("started_at").cloned().unwrap_or(Value::Null),
            l.get("source").cloned().unwrap_or(Value::Null),
        );
    });
    persist_logs(server);
    let (plugin, started, source) = meta;
    audit(
        server,
        "plugin.invocation_finished",
        json!({"plugin": plugin}),
        json!({"kind": "plugin", "id": plugin, "invocation": log_id}),
        json!({
            "log_id": log_id,
            "source": source,
            "status": if ok { "succeeded" } else { "failed" },
            "exit_code": code,
            "duration_ms": started.as_i64().map(|s| now_ms() - s),
        }),
    );
}

/// After a server restart: keep tailing a re-attached invocation's output until its process
/// exits (its exit status is not observable any more).
fn resume_tail(
    server: &Arc<Server>,
    log_id: String,
    out: Option<PathBuf>,
    err: Option<PathBuf>,
    pid: u32,
) {
    let srv = server.clone();
    tokio::spawn(async move {
        let plugin = logs(&srv, None, None)
            .iter()
            .find(|l| l["log_id"] == log_id.as_str())
            .and_then(|l| l["plugin_id"].as_str().map(str::to_string))
            .unwrap_or_default();
        let caps = limits::LogCaps::of(&limits::settings(&plugin));
        let mut tail = Tail::new(out, err, caps);
        while brokers::pid_alive(pid as i32) {
            tail.poll(&srv, &log_id);
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        tail.poll(&srv, &log_id);
        tail.remove();
        let running = logs(&srv, None, None)
            .iter()
            .any(|l| l["log_id"] == log_id.as_str() && l["status"] == "running");
        if running {
            finish_log(
                &srv,
                &log_id,
                true,
                None,
                Some("re-attached after a server restart; exit status unknown"),
            );
        }
    });
}

/// A 0600 output file for an invocation stream.
fn out_file(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

/// Start one plugin process with its private broker and log record; returns the record as it
/// is at launch (status `running`, or `failed` when the spawn itself failed).
fn spawn_invocation(
    server: &Arc<Server>,
    entry: &Entry,
    m: &Manifest,
    command: &[String],
    sp_: Spawn,
    slot: Option<limits::Slot>,
) -> Value {
    let st = state(server);
    let settings = limits::settings(&entry.id);
    let sandboxed = settings.isolate == herdr::settings::Isolate::Sandbox;
    let n = st.next.fetch_add(1, Ordering::Relaxed) + 1;
    let log_id = format!("l{n}-{}", &crate::core::ulid()[20..]);
    let dirs = plugin_dirs();
    let config_dir = dirs.config_dir(&entry.id);
    let state_dir = dirs.state_dir(&entry.id);
    let _ = std::fs::create_dir_all(&config_dir);
    let _ = std::fs::create_dir_all(&state_dir);
    let digest = entry
        .trust
        .as_ref()
        .map(|g| g.manifest_sha256.clone())
        .unwrap_or_default();
    // Herdr's `PluginInvocationContext`, then Vibeke-only keys (Herdr clients ignore them).
    let mut ictx = sp_.ctx.clone();
    if ictx.correlation_id.as_deref().is_none_or(str::is_empty) {
        ictx.correlation_id = Some(log_id.clone());
    }
    let mut context = ictx.to_json();
    context.insert("source".into(), json!(sp_.source));
    context.insert("plugin_id".into(), json!(entry.id));
    context.insert("plugin_version".into(), json!(m.version));
    if let Some(a) = &sp_.action {
        context.insert("action_id".into(), json!(a));
    }
    if let Some((e, _)) = &sp_.event {
        context.insert("event".into(), json!(e));
    }
    let context = Value::Object(context);
    let started = now_ms();
    let mut rec = json!({
        "log_id": log_id,
        "plugin_id": entry.id,
        "action_id": sp_.action,
        "event": sp_.event.as_ref().map(|e| e.0.clone()),
        "entrypoint_id": sp_.entrypoint,
        "source": sp_.source,
        "command": command,
        "long_lived": sp_.long_lived,
        "isolation": if sandboxed { "sandbox" } else { "host" },
        "status": "running",
        "started_unix_ms": started,
        "finished_unix_ms": null,
        "started_at": started,
        "finished_at": null,
        "exit_code": null,
        "stdout": "",
        "stderr": "",
        "context": context,
    });
    let fail = |mut rec: Value, msg: String| {
        rec["status"] = json!("failed");
        rec["stderr"] = json!(vk_redact::redact(&msg));
        rec["finished_at"] = json!(now_ms());
        push_log(server, rec.clone());
        rec
    };
    // A fresh trust check right before anything runs: the caller's view (a hook cache, an
    // earlier registry read) may be stale. The plugin must still be active under the same grant
    // with exactly the manifest this command came from.
    // Fail closed on configuration: an unknown `isolate` value, or a config that cannot be
    // loaded while the plugin was last configured sandboxed, refuses the plugin (never host).
    if let Some(why) = &settings.error {
        return fail(rec, why.clone());
    }
    if let Err(why) = fresh_grant(entry, m) {
        return fail(rec, why);
    }
    // Output goes through a capture helper into 0600 files, bounded by the invocation's
    // budget: the helper (not the server) holds the pipes, so a long-running invocation keeps
    // writing across a server restart, and no invocation can fill the disk.
    let out_dir = compat_root(server).join("out");
    let (out_path, err_path) = (
        out_dir.join(format!("{log_id}.out")),
        out_dir.join(format!("{log_id}.err")),
    );
    let files = private_dir(&out_dir)
        .and_then(|_| {
            out_file(&out_path)?;
            out_file(&err_path)?;
            Ok(())
        })
        .and_then(|_| {
            capture::spawn(
                &server.opts.bin,
                settings.output_max_bytes,
                &out_path,
                &err_path,
            )
        });
    let capture::Capture {
        stdout: out_f,
        stderr: err_f,
        done: mut captured,
    } = match files {
        Ok(f) => f,
        Err(e) => {
            let _ = std::fs::remove_file(&out_path);
            let _ = std::fs::remove_file(&err_path);
            return fail(rec, format!("output: {e}"));
        }
    };
    let token = brokers::new_token();
    // Private broker bound to this invocation's grant.
    let broker = match brokers::new_path(server) {
        Ok(p) => p,
        Err(e) => return fail(rec, format!("broker: {e}")),
    };
    let binding = brokers::Binding {
        path: broker.clone(),
        plugin_id: entry.id.clone(),
        digest,
        default_pane: ictx.focused_pane_id.clone(),
        entrypoint: sp_.entrypoint.clone(),
        source: sp_.source.to_string(),
        log_id: Some(log_id.clone()),
        grant_id: entry
            .trust
            .as_ref()
            .map(|g| g.grant_id.clone())
            .unwrap_or_default(),
        life: brokers::Life::Pending,
        created_at_ms: now_ms(),
        stdout: Some(out_path.clone()),
        stderr: Some(err_path.clone()),
        isolation: Some(if sandboxed { "sandbox" } else { "host" }.into()),
        token: token.clone(),
    };
    if let Err(e) = brokers::bind(server, binding) {
        return fail(rec, format!("broker: {e}"));
    }
    let inv = launch::Invocation {
        plugin_id: entry.id.clone(),
        root: entry.root.clone(),
        config_dir,
        state_dir,
        socket_path: broker.clone(),
        bin_path: launcher(server),
        context,
        // Herdr `start_plugin_command`: ids from the context; the event name for hooks and
        // startup, its envelope for hooks; never an entrypoint id (plugin panes only).
        workspace_id: ictx.workspace_id.clone(),
        tab_id: ictx.tab_id.clone(),
        pane_id: ictx.focused_pane_id.clone(),
        action_id: sp_.action.clone(),
        event: sp_.event.as_ref().map(|e| e.0.clone()),
        event_json: sp_
            .event
            .as_ref()
            .map(|e| e.1.clone())
            .filter(|v| !v.is_null()),
        entrypoint_id: None,
        clicked_url: ictx.clicked_url.clone(),
        link_handler_id: ictx.link_handler_id.clone(),
        broker_token: token,
    };
    let argv = launch::resolve_argv(&entry.root, command);
    // Restricted legacy mode: the argv runs under the `sandbox` level with the plugin dir
    // read-only, its own state dir writable and the network off unless granted; where no
    // working sandbox exists the plugin is refused, never run on the host.
    let mut profile: Option<PathBuf> = None;
    let (argv, env) = if sandboxed {
        let inherited = vk_sandbox::plugin::scrubbed_env(std::env::vars());
        let env = launch::runtime_env(&inv, inherited);
        let mut hidden = vec![crate::paths::state_root(), crate::paths::runtime_root()];
        if let Some(c) = dirs.registry.parent() {
            hidden.push(c.to_path_buf());
        }
        let bx = vk_sandbox::plugin::PluginBox {
            home: crate::paths::home(),
            plugin_root: entry.root.clone(),
            config_dir: inv.config_dir.clone(),
            state_dir: inv.state_dir.clone(),
            hidden,
            // Never `brokers.json`: it names every other invocation's broker.
            extra_read: vec![
                launcher(server)
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_default(),
                server.opts.bin.clone(),
            ],
            sockets: vec![broker.clone()],
            network: settings.network,
            vibeke_bin: Some(server.opts.bin.clone()),
            profile_dir: compat_root(server).join("sbx"),
        };
        match bx.prepare(&argv, &entry.root, env, &log_id) {
            Ok(p) => {
                profile = Some(p.profile.clone());
                (p.argv, p.env)
            }
            Err(e) => {
                brokers::close(server, &broker);
                let _ = std::fs::remove_file(&out_path);
                let _ = std::fs::remove_file(&err_path);
                return fail(rec, e);
            }
        }
    } else {
        let mut env = launch::runtime_env(&inv, std::env::vars());
        // Test-only trace (spec 18 §9): the launcher's invocation id. Restricted invocations
        // above get none; the sandbox would block the trace file anyway.
        env.extend(trace::child_env(Some(log_id.as_str())));
        (argv, env)
    };
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(&entry.root)
        .env_clear()
        .envs(env)
        .stdin(std::process::Stdio::null())
        .stdout(out_f)
        .stderr(err_f)
        // Its own process group: the group is what a long-running entrypoint's lifetime
        // follows, and it survives the server.
        .process_group(0);
    match cmd.spawn() {
        Err(e) => {
            brokers::close(server, &broker);
            let _ = std::fs::remove_file(&out_path);
            let _ = std::fs::remove_file(&err_path);
            if let Some(p) = &profile {
                let _ = std::fs::remove_file(p);
            }
            fail(rec, format!("spawn {}: {e}", argv[0]))
        }
        Ok(mut child) => {
            let pid = child.id().unwrap_or_default();
            rec["pid"] = json!(pid);
            brokers::set_life(
                server,
                &broker,
                if sp_.long_lived {
                    brokers::Life::Group { pid, pgid: pid }
                } else {
                    brokers::Life::Process { pid }
                },
            );
            push_log(server, rec.clone());
            audit(
                server,
                "plugin.invocation_started",
                json!({"plugin": entry.id}),
                json!({"kind": "plugin", "id": entry.id, "invocation": log_id}),
                json!({
                    "log_id": log_id,
                    "source": sp_.source,
                    "action_id": sp_.action,
                    "event": sp_.event.as_ref().map(|e| e.0.clone()),
                    "entrypoint_id": sp_.entrypoint,
                    "pid": pid,
                    "long_lived": sp_.long_lived,
                }),
            );
            let srv = server.clone();
            let id = log_id.clone();
            let long_lived = sp_.long_lived;
            let plugin_id = entry.id.clone();
            let caps = limits::LogCaps::of(&settings);
            tokio::spawn(async move {
                let mut tail = Tail::new(Some(out_path), Some(err_path), caps);
                let status = loop {
                    tokio::select! {
                        s = child.wait() => break s,
                        _ = tokio::time::sleep(Duration::from_millis(150)) => tail.poll(&srv, &id),
                    }
                };
                // Let the capture helper write what the process printed last (it is done at
                // once unless a leftover child still holds the pipes).
                let _ = tokio::time::timeout(Duration::from_millis(500), &mut captured).await;
                tail.poll(&srv, &id);
                // A long-running entrypoint's children may still write; keep the files until
                // the broker (group) closes.
                if !long_lived {
                    tail.remove();
                    brokers::close(&srv, &broker);
                }
                let code = status.as_ref().ok().and_then(|s| s.code());
                let ok = status.as_ref().is_ok_and(|s| s.success());
                finish_log(&srv, &id, ok, code, None);
                if let Some(p) = &profile {
                    let _ = std::fs::remove_file(p);
                }
                // A finished action/hook frees its slot for the next queued hook.
                drop(slot);
                limits::drain(&srv, &plugin_id);
                if long_lived {
                    while brokers::get(&srv, &broker).is_some() {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        tail.poll(&srv, &id);
                    }
                    tail.remove();
                }
            });
            rec
        }
    }
}

/// Re-read the registry: `entry` must still be active under the same grant, with `m` as its
/// current manifest (checked, with the referenced files, by `entry_status`).
fn fresh_grant(entry: &Entry, m: &Manifest) -> Result<(), String> {
    // Under the registry's shared lock: a publish (install/update/trust) in progress is
    // waited for, never observed half-way.
    let reg = Registry::load_shared(&plugin_dirs()).map_err(|e| e.to_string())?;
    let now = reg
        .get(&entry.id)
        .map_err(|_| format!("plugin {} is no longer registered", entry.id))?;
    let (st, fm) = registry::entry_status(now);
    let grant = |e: &Entry| {
        e.trust
            .as_ref()
            .map(|g| (g.grant_id.clone(), g.manifest_sha256.clone()))
    };
    if st != Status::Active
        || fm.as_ref() != Some(m)
        || grant(now) != grant(entry)
        || now.root != entry.root
    {
        return Err(format!(
            "plugin {} changed since it was loaded ({}); not started. Review it with `vibeke plugin trust {} --legacy`",
            entry.id,
            st.as_str(),
            entry.id
        ));
    }
    // The whole managed checkout must still be the reviewed (and built) tree.
    registry::verify_launch(now)
}

// ---- hooks ------------------------------------------------------------------------------------

fn run_startup_hooks(server: &Arc<Server>) {
    let Ok(reg) = Registry::load(&plugin_dirs()) else {
        return;
    };
    let pf = herdr::current_platform();
    // Herdr `run_plugin_startup_hooks`: the current context with source `startup` and
    // correlation `plugin.startup`; `HERDR_PLUGIN_EVENT=startup` without event JSON.
    let mut ctx = snap(server).current_ctx(server, "plugin.startup");
    ctx.invocation_source = Some("startup".into());
    for (entry, m) in reg.active() {
        for (i, s) in m.startup_on(pf).into_iter().enumerate() {
            spawn_invocation(
                server,
                &entry,
                &m,
                &s.command,
                Spawn {
                    source: "startup",
                    action: None,
                    event: Some(("startup".into(), Value::Null)),
                    entrypoint: Some(format!("startup[{i}]")),
                    ctx: ctx.clone(),
                    long_lived: true,
                },
                None,
            );
        }
    }
}

/// Active plugins, re-read when `plugins.json` changes and at most every 2 s otherwise (so a
/// manifest edit takes effect quickly without parsing manifests on every event). The cache only
/// selects candidates: every launch re-verifies trust from disk first (`fresh_grant`).
struct ActiveCache {
    at: Option<std::time::Instant>,
    mtime: Option<std::time::SystemTime>,
    active: Vec<(Entry, Manifest)>,
}

impl ActiveCache {
    fn get(&mut self) -> &[(Entry, Manifest)] {
        let dirs = plugin_dirs();
        let mtime = std::fs::metadata(&dirs.registry)
            .and_then(|m| m.modified())
            .ok();
        let stale =
            self.at.is_none_or(|t| t.elapsed() > Duration::from_secs(2)) || mtime != self.mtime;
        if stale {
            self.active = Registry::load(&dirs)
                .map(|r| r.active())
                .unwrap_or_default();
            self.mtime = mtime;
            self.at = Some(std::time::Instant::now());
        }
        &self.active
    }
}

async fn hook_dispatcher(server: Arc<Server>) {
    let mut rx = server.events.subscribe();
    let mut proj = seeded_projector(&server);
    let mut cache = ActiveCache {
        at: None,
        mtime: None,
        active: vec![],
    };
    let pf = herdr::current_platform();
    loop {
        let ev = match rx.recv().await {
            Ok(e) => e,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(_) => return,
        };
        let projected = project_event(&server, &mut proj, &ev);
        if projected.is_empty() {
            continue;
        }
        let active = cache.get().to_vec();
        if active.is_empty() {
            continue;
        }
        for (name, _pane, data) in projected {
            // Herdr runs hooks for `PLUGIN_HOOK_EVENT_KINDS` only.
            if !hev::HOOK_EVENTS.contains(&name) {
                continue;
            }
            if !active
                .iter()
                .any(|(_, m)| !m.hooks_for(name, pf).is_empty())
            {
                continue;
            }
            // Herdr `plugin_context_for_event`, and the whole `EventEnvelope` as JSON.
            let ctx = snap(&server).ctx_for_event(&server, name, &data);
            let envelope = hev::envelope(name, data);
            for (entry, m) in &active {
                for (i, h) in m.hooks_for(name, pf).into_iter().enumerate() {
                    limits::dispatch_hook(
                        &server,
                        limits::Queued {
                            entry: entry.clone(),
                            m: m.clone(),
                            command: h.command.clone(),
                            source: "event".into(),
                            event: Some((name.to_string(), envelope.clone())),
                            entrypoint: Some(
                                h.id.clone().unwrap_or_else(|| format!("events[{i}]")),
                            ),
                            ctx: ctx.clone(),
                        },
                    );
                }
            }
        }
    }
}

// ---- native API -------------------------------------------------------------------------------

fn wire_to_rpc(e: WireError) -> vk_proto::rpc::RpcError {
    let kind = match e.code.as_str() {
        "permission_denied" => ErrorKind::PermissionDenied,
        c if c.ends_with("not_found") && c != "method_not_found" => ErrorKind::NotFound,
        "invalid_params" | "invalid_request" => ErrorKind::InvalidParams,
        "timeout" => ErrorKind::Timeout,
        "unsupported" => ErrorKind::Unsupported,
        "method_not_found" | "unknown_method" => ErrorKind::MethodNotFound,
        _ => ErrorKind::Internal,
    };
    err(kind, e.message).details(json!({"herdr_code": e.code}))
}

/// Native methods: `plugin.list`, `plugin.action.list`, `plugin.action.run`, `plugin.log.list`,
/// `compat.herdr.call` (the CLI shim's transport) and `compat.status`.
pub async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    let s = |k: &str| p.get(k).and_then(Value::as_str);
    Some(match method {
        "plugin.list" => Ok(json!({"plugins": plugin_list()})),
        "plugin.action.list" => action_list(s("plugin"))
            .map(|a| {
                let keys = Registry::load(&plugin_dirs())
                    .map(|r| views::keybindings(&r))
                    .unwrap_or_default();
                json!({"actions": a, "keybindings": keys})
            })
            .map_err(wire_to_rpc),
        // The CLI (or any registry editor) tells a running server that plugins.json changed:
        // views of plugins that are no longer active are dropped and clients re-read key
        // bindings and actions.
        "plugin.registry.notify" => {
            if ctx.pane_scope.is_some() {
                return Some(Err(err(
                    ErrorKind::PermissionDenied,
                    "not available from a pane",
                )));
            }
            views::registry_changed(server);
            Ok(json!({"ok": true}))
        }
        "plugin.action.run" => (|| {
            let (plugin, action) = match (s("plugin"), s("action")) {
                (Some(pl), Some(a)) => (pl.to_string(), a.to_string()),
                (None, Some(q)) => split_qualified(q)?,
                _ => {
                    return Err(WireError::new(
                        "invalid_params",
                        "plugin and action are required",
                    ));
                }
            };
            // Where the user started it (TUI palette, key binding); anything else is `cli`.
            let source = s("source")
                .filter(|x| matches!(*x, "palette" | "keybinding"))
                .unwrap_or("cli");
            // Herdr uses the correlation id `keybinding` for key bindings; other runs get
            // their log id.
            let sn = snap(server);
            let corr = if source == "keybinding" {
                "keybinding"
            } else {
                ""
            };
            let ictx = InvokeContext::from_params(server, &sn, p, source, corr);
            invoke_action(
                server,
                ctx.pane_scope.as_deref(),
                &plugin,
                &action,
                ictx,
                source,
            )
            .map(|log| json!({"log": log}))
        })()
        .map_err(wire_to_rpc),
        "plugin.log.list" => {
            Ok(json!({"logs": logs(server, s("plugin"), p.get("limit").and_then(Value::as_u64))}))
        }
        "plugin.link_handler.list" => link_handler_list()
            .map(|h| json!({"handlers": h}))
            .map_err(wire_to_rpc),
        "plugin.link.open" => open_link(server, ctx, p)
            .map(|log| json!({"log": log}))
            .map_err(wire_to_rpc),
        // The TUI dismisses a popup/overlay (esc after exit, prefix+x): close and give the
        // focus back as the compat `popup.close` does.
        "plugin.surface.close" => {
            if ctx.pane_scope.is_some() {
                return Some(Err(err(
                    ErrorKind::PermissionDenied,
                    "plugin surfaces cannot be closed from a pane",
                )));
            }
            let Some(pane) = s("pane") else {
                return Some(Err(invalid("pane is required")));
            };
            // Idempotent: the surface may already be gone (its command exited).
            match server.with_core(|c| c.pane(pane).map(|x| x.plugin_surface().is_some())) {
                None => Ok(json!({"closed": null})),
                Some(false) => Err(invalid("not a plugin popup or overlay")),
                Some(true) => {
                    ext::close_surface(server, pane);
                    Ok(json!({"closed": pane}))
                }
            }
        }
        // Plugin-set UI state a client shows (window title override, open popup).
        "compat.ui.state" => {
            let title = state(server).meta.lock().unwrap().window_title.clone();
            let popup = ext::open_popup(server).map(|(id, pp)| {
                json!({"pane": id, "plugin_id": pp.plugin, "entrypoint_id": pp.entrypoint})
            });
            Ok(json!({"window_title": title, "popup": popup, "agent_views": views::list(server)}))
        }
        "compat.herdr.call" => {
            let Some(m) = s("method") else {
                return Some(Err(invalid("method is required")));
            };
            let params = p.get("params").cloned().unwrap_or_else(|| json!({}));
            let params = if params.is_object() {
                params
            } else {
                json!({})
            };
            // A plugin invocation that selected this session (`herdr --session`) keeps its
            // plugin identity, proven by a single-use ticket from its own broker and verified
            // with the issuing session; the grant is then re-checked here (09 §6). A bare plugin
            // id is never accepted.
            let mut caller = Caller::user(ctx.clone());
            if let Some(ap) = p.get("as_plugin") {
                let denied = |msg: String| {
                    Some(Ok(
                        json!({"error": {"code": "permission_denied", "message": msg}}),
                    ))
                };
                if ctx.pane_scope.is_some() {
                    return denied("a pane cannot act as a plugin".into());
                }
                let (Some(session), Some(ticket)) = (
                    ap.get("session").and_then(Value::as_str),
                    ap.get("ticket").and_then(Value::as_str),
                ) else {
                    return denied(
                        "plugin identity needs a ticket from the invocation's broker".into(),
                    );
                };
                let v = match verify_ticket(server, session, ticket).await {
                    Ok(v) => v,
                    Err(e) => return denied(format!("plugin identity not verified: {e}")),
                };
                let id = v["plugin_id"].as_str().unwrap_or_default().to_string();
                let digest = v["digest"].as_str().unwrap_or_default().to_string();
                let grant_id = v["grant_id"].as_str().unwrap_or_default().to_string();
                if !brokers::grant_ok(&id, &digest, &grant_id) {
                    return denied(format!("plugin {id} is not trusted and enabled"));
                }
                caller.plugin = Some((id, digest));
                caller.grant_id = Some(grant_id);
                caller.cross_session = true;
                // Fail closed: an answer without the field is treated as restricted.
                caller.sandboxed = v["sandboxed"].as_bool() != Some(false);
                caller.invocation = v["log_id"].as_str().map(|l| format!("{session}/{l}"));
                caller.ctx.kind = "plugin".into();
            }
            let r = call(server, &caller, m, &params).await;
            audit_call(
                server,
                &caller,
                m,
                r.as_ref().map(|_| ()).map_err(|e| e.code.as_str()),
            );
            // Only the shim calls this method; traced for plugin identities (spec 18 §9).
            trace_result(
                &caller,
                "cli",
                m,
                Some(&params),
                r.as_ref().map(|_| ()).map_err(|e| e.code.as_str()),
            );
            Ok(match r {
                Ok(v) => json!({"result": v}),
                Err(e) => json!({"error": {"code": e.code, "message": e.message}}),
            })
        }
        "compat.invocation.verify" => {
            if ctx.pane_scope.is_some() {
                return Some(Err(err(
                    ErrorKind::PermissionDenied,
                    "not available from a pane",
                )));
            }
            let Some(t) = s("ticket") else {
                return Some(Err(invalid("ticket is required")));
            };
            redeem_ticket(server, t).map_err(|m| err(ErrorKind::PermissionDenied, m))
        }
        "compat.status" => {
            let (i, pa, mi) = inventory::counts(None);
            let path = listener_path(server);
            Ok(json!({
                "baseline": {"herdr": herdr::BASELINE_VERSION, "commit": herdr::BASELINE_COMMIT},
                "support": "partial",
                "listener": {"enabled": compat_enabled(), "path": path, "live": path.exists()},
                "inventory": {"implemented": i, "partial": pa, "missing": mi},
                "brokers": brokers::all(server).len(),
                "herdr_root": herdr_root(server),
                "registry": plugin_dirs().registry,
            }))
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Paths;
    use crate::{Server, ServerOpts};
    use std::sync::Once;

    fn init_env() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            // Same values as the other in-process server tests (one root per test binary).
            let base = std::env::temp_dir().join(format!("vk-review-tests-{}", std::process::id()));
            std::fs::create_dir_all(&base).unwrap();
            // SAFETY: identical values to the other writers; set before servers read them.
            unsafe {
                std::env::set_var("VIBEKE_RUNTIME_DIR", base.join("run"));
                std::env::set_var("VIBEKE_STATE_DIR", base.join("state"));
                std::env::set_var("VIBEKE_CONFIG", base.join("config.toml"));
            }
        });
    }

    #[tokio::test]
    async fn gap_methods_dispatch_and_stay_gated() {
        let (srv, _d) = server();
        // `agent.explain` is a known method that needs a real target.
        let e = call(&srv, &user(), "agent.explain", &json!({"target": "w9:p9"}))
            .await
            .unwrap_err();
        assert_eq!(e.code, "agent_not_found");
        let e = call(&srv, &user(), "agent.explain", &json!({}))
            .await
            .unwrap_err();
        assert_eq!(e.code, "invalid_params");
        let v = call(&srv, &user(), "api.schema", &json!({})).await.unwrap();
        assert!(
            v["methods"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["name"] == "agent.explain")
        );
        // `server.stop` is off unless the config enables it; a plugin is refused regardless.
        let e = call(&srv, &user(), "server.stop", &json!({}))
            .await
            .unwrap_err();
        assert_eq!(e.code, "unsupported");
        // The pane environment carries no HERDR_* while the compat listener is off.
        let env = srv.pane_env_for("p1", "w1:p1", "w1:t1", "w1", &[]);
        assert!(env.iter().all(|(k, _)| !k.starts_with("HERDR_")), "{env:?}");
    }

    fn server() -> (Arc<Server>, tempfile::TempDir) {
        init_env();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let paths = Paths {
            session: "t".into(),
            runtime: root.join("run"),
            state: root.join("state"),
        };
        let opts = ServerOpts {
            session: "t".into(),
            machine: "testbox".into(),
            bin: "/bin/false".into(),
            hold_args: vec![],
            default_shell: None,
            env: vec![],
            shims: false,
            gateway: None,
        };
        (Server::new(paths, opts).unwrap(), dir)
    }

    fn user() -> Caller {
        Caller::user(Ctx {
            client_id: "c-user".into(),
            kind: "cli".into(),
            pane_scope: None,
            remote: false,
        })
    }

    #[test]
    fn output_matches_whole_lines_like_herdr() {
        let text = "$ make\nbuilding\nbuild ok: 3 targets\n$ ";
        assert_eq!(
            match_line(text, Some("ok:"), None).as_deref(),
            Some("build ok: 3 targets")
        );
        let re = regex::Regex::new(r"^\$ $").unwrap();
        assert_eq!(match_line(text, None, Some(&re)).as_deref(), Some("$ "));
        assert_eq!(match_line(text, Some("missing"), None), None);
        assert_eq!(match_line(text, None, None), None);
    }

    #[tokio::test]
    async fn inventory_status_matches_dispatch() {
        let (srv, _d) = server();
        // Creating workspaces/tabs needs holders; those are covered end to end.
        let skip = ["workspace.create", "tab.create", "events.subscribe"];
        for e in inventory::ENTRIES
            .iter()
            .filter(|e| e.kind == inventory::Kind::Method)
        {
            if skip.contains(&e.name) {
                continue;
            }
            let r = call(&srv, &user(), e.name, &json!({})).await;
            let code = r.as_ref().err().map(|e| e.code.clone());
            // Served (partial) but off by default: `[compat.herdr] allow_server_stop = false`
            // answers `unsupported` (spec 07 §8.3). The enabled path is covered by
            // `gap_methods_dispatch_and_stay_gated` and the `gap` unit tests.
            if e.name == "server.stop" {
                assert_eq!(e.status, inventory::Status::Partial);
                assert_eq!(code.as_deref(), Some("unsupported"), "{r:?}");
                continue;
            }
            match e.status {
                inventory::Status::Missing => assert_eq!(
                    code.as_deref(),
                    Some("unsupported"),
                    "{} is listed missing but dispatches: {r:?}",
                    e.name
                ),
                _ => assert!(
                    !matches!(code.as_deref(), Some("unsupported" | "method_not_found")),
                    "{} is listed {} but returns {code:?}",
                    e.name,
                    e.status.as_str()
                ),
            }
        }
        let r = call(&srv, &user(), "galaxy.explode", &json!({})).await;
        assert_eq!(r.unwrap_err().code, "invalid_request");
        let r = call(&srv, &user(), "pane.graphics.info", &json!({})).await;
        assert_eq!(r.unwrap_err().code, "unknown_method");
    }

    #[tokio::test]
    async fn shapes_and_errors_on_an_empty_session() {
        let (srv, _d) = server();
        let v = call(&srv, &user(), "workspace.list", &json!({}))
            .await
            .unwrap();
        assert_eq!(v, json!({"type": "workspace_list", "workspaces": []}));
        let v = call(&srv, &user(), "session.snapshot", &json!({}))
            .await
            .unwrap();
        assert_eq!(v["type"], "session_snapshot");
        let snap = &v["snapshot"];
        assert_eq!(snap["version"], herdr::BASELINE_VERSION);
        assert_eq!(snap["protocol"], herdr::cli::PROTOCOL_VERSION);
        assert!(snap.get("focused_pane_id").is_none(), "{v}");
        for k in ["workspaces", "tabs", "panes", "layouts", "agents"] {
            assert!(snap[k].as_array().is_some_and(|a| a.is_empty()), "{k}: {v}");
        }
        let v = call(&srv, &user(), "ping", &json!({})).await.unwrap();
        assert_eq!(v["type"], "pong");
        assert_eq!(v["protocol"], herdr::cli::PROTOCOL_VERSION);
        assert_eq!(v["capabilities"]["live_handoff"], false);
        // Herdr's `events.wait {match_event}` waits only for agent status matches.
        let e = call(
            &srv,
            &user(),
            "events.wait",
            &json!({"match_event": {"event": "tab_created"}}),
        )
        .await
        .unwrap_err();
        assert_eq!(e.code, "unsupported_event_wait_match");
        let e = call(
            &srv,
            &user(),
            "events.wait",
            &json!({"match_event": {"event": "pane_agent_status_changed", "pane_id": "w9:p9", "agent_status": "idle"}, "timeout_ms": 10}),
        )
        .await
        .unwrap_err();
        assert_eq!(e.code, "pane_not_found");
        let e = call(
            &srv,
            &user(),
            "workspace.focus",
            &json!({"workspace_id": "w42"}),
        )
        .await
        .unwrap_err();
        assert_eq!(e.code, "workspace_not_found");
        let e = call(&srv, &user(), "pane.read", &json!({"pane_id": "w1:p1"}))
            .await
            .unwrap_err();
        assert_eq!(e.code, "pane_not_found");
        let e = call(&srv, &user(), "pane.send_text", &json!({}))
            .await
            .unwrap_err();
        assert_eq!(e.code, "pane_not_found", "no pane and no focus");
        let e = call(
            &srv,
            &user(),
            "events.wait",
            &json!({"subscriptions": [{"type": "pane.output_matched"}]}),
        )
        .await
        .unwrap_err();
        assert_eq!(e.code, "invalid_params");
        let v = call(&srv, &user(), "api.schema", &json!({})).await.unwrap();
        assert!(
            v["methods"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["name"] == "pane.send_text" && m["status"] == "implemented")
        );
        let e = call(&srv, &user(), "server.stop", &json!({}))
            .await
            .unwrap_err();
        assert_eq!(e.code, "unsupported");
    }

    #[tokio::test]
    async fn native_api_and_no_escalation() {
        let (srv, _d) = server();
        let user_ctx = user().ctx;
        let r = api::dispatch(
            &srv,
            &user_ctx,
            "compat.herdr.call",
            &json!({"method": "ping"}),
        )
        .await
        .unwrap();
        assert_eq!(r["result"]["type"], "pong");
        let r = api::dispatch(
            &srv,
            &user_ctx,
            "compat.herdr.call",
            &json!({"method": "nope"}),
        )
        .await
        .unwrap();
        assert_eq!(r["error"]["code"], "invalid_request");
        let st = api::dispatch(&srv, &user_ctx, "compat.status", &json!({}))
            .await
            .unwrap();
        assert_eq!(st["support"], "partial");
        assert_eq!(st["listener"]["live"], false);
        // A pane-scoped caller cannot invoke a legacy plugin, whatever its trust state.
        let e = invoke_action(
            &srv,
            Some("p-agent"),
            "acme.any",
            "go",
            InvokeContext::default(),
            "api",
        )
        .unwrap_err();
        assert_eq!(e.code, "permission_denied");
        let e = invoke_action(
            &srv,
            None,
            "acme.not-installed",
            "go",
            InvokeContext::default(),
            "api",
        )
        .unwrap_err();
        assert_eq!(e.code, "plugin_not_found");
        // A broker whose plugin is gone is rejected on every request.
        let ghost = Caller {
            plugin: Some(("acme.ghost".into(), "00".into())),
            ..user()
        };
        assert_eq!(
            check_broker(&srv, &ghost).unwrap_err().code,
            "permission_denied"
        );
        assert!(check_broker(&srv, &user()).is_ok());
    }

    #[tokio::test]
    async fn listener_uses_herdrs_session_layout() {
        let (srv, d) = server();
        let root = d.path().canonicalize().unwrap();
        assert_eq!(herdr_root(&srv), root.join("herdr-compat"));
        assert_eq!(
            listener_path(&srv),
            root.join("herdr-compat/sessions/t/herdr.sock"),
            "a named session lives under sessions/<name>/"
        );
        assert!(compat_root(&srv).starts_with(root.join("run")));
    }

    #[tokio::test]
    async fn recovery_drops_bindings_without_a_live_process_or_a_grant() {
        let (srv, _d) = server();
        private_dir(&compat_root(&srv)).unwrap();
        let sock = |n: &str| compat_root(&srv).join(format!("{n}.sock"));
        let b = |n: &str, plugin: &str, life: brokers::Life| brokers::Binding {
            path: sock(n),
            plugin_id: plugin.into(),
            digest: "d".into(),
            default_pane: None,
            entrypoint: None,
            source: "startup".into(),
            log_id: None,
            grant_id: String::new(),
            life,
            created_at_ms: 0,
            stdout: None,
            stderr: None,
            isolation: Some("host".into()),
            token: String::new(),
        };
        let me = std::process::id();
        let mut legacy = b("legacy", "acme.gone", brokers::Life::Process { pid: me });
        legacy.isolation = None;
        let list = vec![
            // Persisted by an older version (no isolation recorded): never re-issued.
            legacy,
            // Alive, but the plugin is not registered (no grant): dropped.
            b("ungranted", "acme.gone", brokers::Life::Process { pid: me }),
            // Dead process: dropped.
            b(
                "dead",
                "acme.gone",
                brokers::Life::Process { pid: 0x7fff_fff0 },
            ),
            // Never started: dropped.
            b("pending", "acme.gone", brokers::Life::Pending),
        ];
        for x in &list {
            std::fs::write(&x.path, "").unwrap();
        }
        std::fs::write(
            compat_root(&srv).join("brokers.json"),
            serde_json::to_vec(&list).unwrap(),
        )
        .unwrap();
        assert_eq!(brokers::recover(&srv), (0, 4));
        for x in &list {
            assert!(
                !x.path.exists(),
                "stale socket {} removed",
                x.path.display()
            );
        }
        assert!(brokers::all(&srv).is_empty());
        assert!(brokers::pid_alive(me as i32));
    }

    #[tokio::test]
    async fn a_sandboxed_broker_serves_only_the_restricted_allowlist() {
        let (srv, _d) = server();
        let sbx = Caller {
            sandboxed: true,
            ..user()
        };
        for m in [
            "pane.run",
            "pane.send_text",
            "pane.send_keys",
            "pane.send_input",
            "pane.read",
            "pane.process_info",
            "agent.send",
            "agent.prompt",
            "agent.start",
            "tab.create",
            "workspace.create",
            "pane.split",
            "pane.report_metadata",
            "plugin.pane.open",
            "plugin.enable",
            "worktree.create",
            "worktree.repo_root",
            "vibeke.invocation_ticket",
        ] {
            assert!(!sandbox_allows(m), "{m}");
            let e = call(&srv, &sbx, m, &json!({})).await.unwrap_err();
            assert_eq!(e.code, "permission_denied", "{m}: {e}");
            assert!(e.message.contains("restricted (sandboxed)"), "{m}: {e}");
        }
        for m in ["workspace.list", "pane.list", "session.snapshot", "ping"] {
            assert!(sandbox_allows(m), "{m}");
            assert!(call(&srv, &sbx, m, &json!({})).await.is_ok(), "{m}");
        }
        // Another plugin's action is refused before anything is looked up.
        let sbx_plugin = Caller {
            plugin: Some(("acme.sbx".into(), "d".into())),
            ..sbx.clone()
        };
        let e = call(
            &srv,
            &sbx_plugin,
            "plugin.action.invoke",
            &json!({"plugin_id": "acme.host", "action_id": "go"}),
        )
        .await
        .unwrap_err();
        assert!(e.message.contains("only its own actions"), "{e}");
        // Host-mode callers are unaffected by the policy.
        assert!(!user().sandboxed);
    }

    #[test]
    fn broker_tokens_compare_exactly() {
        let t = brokers::new_token();
        assert_eq!(t.len(), 64);
        assert_ne!(t, brokers::new_token());
        assert!(same_secret(&t, &t.clone()));
        assert!(!same_secret(&t, &t[..63]));
        assert!(!same_secret(&t, &brokers::new_token()));
        assert!(!same_secret("", "x"));
    }

    #[tokio::test]
    async fn a_broker_belongs_to_its_process_tree_and_group() {
        let (srv, _d) = server();
        let me = std::process::id();
        let parent = vk_hold::procinfo::info(me).unwrap().ppid;
        let mine = brokers::Life::Process { pid: me };
        assert!(brokers::peer_belongs(&srv, &mine, Some(me as i32)));
        // A child of the invocation belongs; its parent (or anything else) does not.
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("5")
            .spawn()
            .unwrap();
        assert!(brokers::peer_belongs(&srv, &mine, Some(child.id() as i32)));
        assert!(!brokers::peer_belongs(
            &srv,
            &brokers::Life::Process { pid: child.id() },
            Some(me as i32)
        ));
        assert!(!brokers::peer_belongs(&srv, &mine, Some(parent as i32)));
        assert!(!brokers::peer_belongs(&srv, &mine, None));
        assert!(!brokers::peer_belongs(
            &srv,
            &brokers::Life::Pending,
            Some(me as i32)
        ));
        // Long-running entrypoints: any member of the process group.
        let pgid = vk_hold::procinfo::info(me).unwrap().pgid;
        let group = brokers::Life::Group {
            pid: 0x7fff_fff0,
            pgid,
        };
        assert!(brokers::peer_belongs(&srv, &group, Some(me as i32)));
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn plugin_log_tails_are_redacted() {
        let token = format!("ghp_{}", "A".repeat(36));
        let out = redacted(format!("pushing with {token}\n").as_bytes(), 0);
        assert!(!out.contains(&token), "{out}");
        assert!(out.contains("pushing with"));
        let caps = limits::LogCaps {
            bytes: 100,
            lines: 1000,
        };
        let mut big = vec![b'x'; 110];
        assert_eq!(tail_keep(&mut big, caps), 10);
        assert_eq!(big.len(), 100);
    }

    #[tokio::test]
    async fn read_only_methods_are_not_audited() {
        assert!(read_only("pane.list") && read_only("layout.export"));
        assert!(!read_only("pane.send_text") && !read_only("pane.move"));
        let (srv, _d) = server();
        let plugin = Caller {
            plugin: Some(("acme.audit".into(), "d".into())),
            invocation: Some("l1".into()),
            ..user()
        };
        audit_call(&srv, &plugin, "pane.list", Ok(()));
        audit_call(&srv, &plugin, "pane.rename", Err("pane_not_found"));
        audit_call(&srv, &user(), "pane.rename", Ok(()));
        let evs = srv.with_core(|c| c.store.events_after(0, 100, &[]).unwrap());
        let calls: Vec<_> = evs.iter().filter(|e| e.kind == "plugin.api_call").collect();
        assert_eq!(calls.len(), 1, "only the plugin's mutating call");
        assert_eq!(calls[0].actor["kind"], "plugin");
        assert_eq!(calls[0].actor["id"], "acme.audit");
        assert_eq!(calls[0].data["method"], "pane.rename");
        assert_eq!(calls[0].data["error_code"], "pane_not_found");
    }

    fn model_pane(id: &str, handle: &str, created_by: &str) -> Pane {
        serde_json::from_value(json!({
            "id": id, "handle": handle, "tab": "T", "workspace": "W", "title": null,
            "auto_title": "sh", "cwd": null, "cols": 80, "rows": 24, "child_pid": null,
            "fg_cmdline": [], "exited": false, "exit_code": null, "unread": false,
            "marked_unread": false, "pinned": false, "created_by": created_by, "recovered": null
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn popups_are_hidden_and_keep_the_focus_underneath() {
        let (srv, _d) = server();
        let tag = "plugin-surface:popup:80%:20:acme.demo/pick";
        srv.with_core(|c| {
            c.model.panes.push(model_pane("P1", "w1:p1", "user"));
            c.model.panes.push(model_pane("POP", "w1:p2", tag));
            c.model.panes.push(model_pane(
                "OV",
                "w1:p3",
                "plugin-surface:overlay:::acme.demo/b",
            ));
        });
        state(&srv).meta.lock().unwrap().plugin_panes.insert(
            "POP".into(),
            ext::PluginPane {
                plugin: "acme.demo".into(),
                entrypoint: "pick".into(),
                placement: "popup".into(),
                prev_focus: Some("P1".into()),
            },
        );
        srv.clients
            .lock()
            .unwrap()
            .insert("c-tui".into(), Default::default());
        if let Some(st) = srv.clients.lock().unwrap().get_mut("c-tui") {
            st.kind = "tui".into();
            st.focus.pane = Some("POP".into());
        }
        let sn = snap(&srv);
        let ids: Vec<&str> = sn.panes.iter().map(|p| p.handle.as_str()).collect();
        assert_eq!(
            ids,
            ["w1:p1", "w1:p3"],
            "the popup is no pane; the overlay is"
        );
        assert_eq!(sn.focus.pane.as_deref(), Some("P1"), "focus underneath");
        // Its lifecycle events project to nothing, even after it is gone from the model.
        state(&srv).meta.lock().unwrap().popups.insert("POP".into());
        srv.with_core(|c| c.model.panes.retain(|p| p.id != "POP"));
        let mut proj = Projector::new();
        for kind in ["pane.created", "pane.closed", "pane.focused", "pane.exited"] {
            let ev = vk_store::Event {
                seq: 1,
                ts: 0,
                v: 1,
                tier: "sync".into(),
                kind: kind.into(),
                subject: json!({"pane": "POP", "pane_handle": "w1:p2"}),
                actor: json!({}),
                data: json!({}),
            };
            assert!(project_event(&srv, &mut proj, &ev).is_empty(), "{kind}");
        }
        // A scroll report is remembered and projected for ordinary panes.
        scroll_changed(&srv, "c-tui", "P1", 12, 300);
        scroll_changed(&srv, "c-tui", "P1", 12, 300);
        scroll_changed(&srv, "c-tui", "nope", 3, 3);
        let evs = srv.with_core(|c| c.store.events_after(0, 100, &[]).unwrap());
        let sc: Vec<_> = evs
            .iter()
            .filter(|e| e.kind == "pane.scroll_changed")
            .collect();
        assert_eq!(sc.len(), 1, "deduplicated; unknown panes ignored");
        assert_eq!(sc[0].data["offset"], 12);
        let out = project_event(&srv, &mut proj, sc[0]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "pane.scroll_changed");
        assert_eq!(out[0].1.as_deref(), Some("w1:p1"));
        assert_eq!(out[0].2["scroll"]["offset_from_bottom"], 12);
        let sn = snap(&srv);
        let p1 = sn.pane("P1").unwrap();
        // Herdr omits `scroll` without terminal metrics (this pane has no runtime).
        assert!(sn.pane_json(&srv, p1).get("scroll").is_none());
        assert_eq!(state(&srv).meta.lock().unwrap().scroll.get("P1"), Some(&12));
        scroll_changed(&srv, "c-tui", "P1", 0, 300);
        assert_eq!(state(&srv).meta.lock().unwrap().scroll.get("P1"), None);
    }

    /// A workspace `w1` (root `repo`, a git checkout) with tab `w1:t1` holding panes `w1:p1`
    /// and `w1:p2` (the tab's own focus); no client attached.
    fn one_workspace(srv: &Server, root: &str) {
        srv.with_core(|c| {
            c.model.workspaces.push(Workspace {
                id: "W".into(),
                handle: "w1".into(),
                name: Some("demo".into()),
                auto_name: "auto".into(),
                root_path: root.into(),
                task: None,
                order: 0.0,
                branch: None,
            });
            c.model.tabs.push(Tab {
                id: "T".into(),
                handle: "w1:t1".into(),
                workspace: "W".into(),
                title: None,
                number: 1,
                layout: LayoutNode::Leaf { pane: "P1".into() },
                focused_pane: Some("P2".into()),
                zoomed_pane: None,
                order: 0.0,
                floating: vec![],
                floats_hidden: false,
            });
            c.model.panes.push(model_pane("P1", "w1:p1", "user"));
            c.model.panes.push(model_pane("P2", "w1:p2", "user"));
        });
    }

    fn event(kind: &str, subject: Value) -> vk_store::Event {
        vk_store::Event {
            seq: 1,
            ts: 0,
            v: 1,
            tier: "sync".into(),
            kind: kind.into(),
            subject,
            actor: json!({}),
            data: json!({}),
        }
    }

    #[tokio::test]
    async fn contexts_follow_herdr_without_a_client() {
        let (srv, d) = server();
        let repo = d.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let root = repo.to_string_lossy().into_owned();
        one_workspace(&srv, &root);

        // Herdr `current_plugin_context`: the active workspace, its tab and focused pane.
        let sn = snap(&srv);
        let c = sn.current_ctx(&srv, "corr");
        assert_eq!(c.workspace_id.as_deref(), Some("w1"));
        assert_eq!(c.workspace_label.as_deref(), Some("demo"));
        assert_eq!(c.workspace_cwd.as_deref(), Some(root.as_str()));
        assert_eq!(c.tab_id.as_deref(), Some("w1:t1"));
        assert_eq!(c.tab_label.as_deref(), Some("1"));
        assert_eq!(
            c.focused_pane_id.as_deref(),
            Some("w1:p2"),
            "the tab's focus"
        );
        assert_eq!(c.focused_pane_status.as_deref(), Some("unknown"));
        assert_eq!(c.invocation_source.as_deref(), Some("api"));
        assert_eq!(c.correlation_id.as_deref(), Some("corr"));
        let wt = c.worktree.clone().unwrap();
        assert_eq!(wt["checkout_path"], root.as_str());
        assert_eq!(wt["repo_root"], root.as_str());
        assert_eq!(wt["repo_name"], "repo");
        assert_eq!(wt["is_linked_worktree"], false);
        let keys: Vec<String> = c.to_json().keys().cloned().collect();
        assert_eq!(
            keys,
            [
                "workspace_id",
                "workspace_label",
                "workspace_cwd",
                "worktree",
                "tab_id",
                "tab_label",
                "focused_pane_id",
                "focused_pane_status",
                "invocation_source",
                "correlation_id",
            ],
            "Herdr's order; absent fields left out"
        );

        // A focus change is remembered without a client (`pane.focused`).
        let mut proj = Projector::new();
        let out = project_event(
            &srv,
            &mut proj,
            &event(
                "pane.focused",
                json!({"pane": "P1", "pane_handle": "w1:p1", "tab": "T", "workspace": "W"}),
            ),
        );
        let names: Vec<&str> = out.iter().map(|o| o.0).collect();
        assert_eq!(names, ["workspace.focused", "tab.focused", "pane.focused"]);
        assert_eq!(out[2].2, json!({"pane_id": "w1:p1", "workspace_id": "w1"}));
        assert_eq!(out[1].2, json!({"tab_id": "w1:t1", "workspace_id": "w1"}));
        let sn = snap(&srv);
        let c = sn.current_ctx(&srv, "corr");
        assert_eq!(c.focused_pane_id.as_deref(), Some("w1:p1"));
        assert_eq!(sn.focused().2.as_deref(), Some("w1:p1"));
        let p1 = sn.pane("P1").unwrap();
        let pj = sn.pane_json(&srv, p1);
        assert_eq!(pj["focused"], true);
        assert_eq!(pj["agent_status"], "unknown");
        assert!(pj.get("cwd").is_none(), "absent, not null: {pj}");
        let p2 = sn.pane("P2").unwrap();
        assert_eq!(sn.pane_json(&srv, p2)["focused"], false);
        let w = sn.ws_json(sn.ws("W").unwrap());
        assert_eq!(w["focused"], true);
        assert_eq!(w["agent_status"], "unknown");
        assert_eq!(w["active_tab_id"], "w1:t1");
        assert_eq!(w["worktree"]["repo_name"], "repo");
        let t = sn.tab_json(sn.tab("T").unwrap());
        assert_eq!(t["label"], "1");
        assert_eq!(t["agent_status"], "unknown");
        assert_eq!(t["focused"], true);

        // Herdr `merge_plugin_context`: the caller's fields replace the current ones.
        let m = InvokeContext::from_params(
            &srv,
            &sn,
            &json!({"context": {"workspace_id": "w9", "selected_text": "hi", "invocation_source": "cli"}}),
            "api",
            "c1",
        );
        assert_eq!(m.workspace_id.as_deref(), Some("w9"));
        assert_eq!(m.focused_pane_id.as_deref(), Some("w1:p1"), "kept");
        assert_eq!(m.selected_text.as_deref(), Some("hi"));
        assert_eq!(m.invocation_source.as_deref(), Some("cli"));
        assert_eq!(m.correlation_id.as_deref(), Some("c1"));
        // Vibeke's own callers may name the pane.
        let m = InvokeContext::from_params(&srv, &sn, &json!({"pane": "w1:p2"}), "cli", "");
        assert_eq!(m.focused_pane_id.as_deref(), Some("w1:p2"));
        assert_eq!(m.tab_id.as_deref(), Some("w1:t1"));

        // Herdr `plugin_context_for_event`.
        let c = sn.ctx_for_event(
            &srv,
            "pane.closed",
            &json!({"pane_id": "w1:p7", "workspace_id": "w1"}),
        );
        assert_eq!(c.focused_pane_id.as_deref(), Some("w1:p7"));
        assert_eq!(c.workspace_id.as_deref(), Some("w1"));
        assert!(c.workspace_label.is_none() && c.tab_id.is_none());
        assert_eq!(c.correlation_id.as_deref(), Some("pane.closed"));
        let c = sn.ctx_for_event(
            &srv,
            "tab.created",
            &json!({"tab": {"tab_id": "w1:t1", "workspace_id": "w1"}}),
        );
        assert_eq!(c.tab_id.as_deref(), Some("w1:t1"));
        assert_eq!(c.workspace_label.as_deref(), Some("demo"));

        // Close events keep the closed object's Herdr id.
        srv.with_core(|c| c.model.panes.retain(|p| p.id != "P2"));
        let out = project_event(
            &srv,
            &mut proj,
            &event(
                "pane.closed",
                json!({"pane": "P2", "tab": "T", "workspace": "W"}),
            ),
        );
        assert_eq!(out[0].0, "pane.closed");
        assert_eq!(out[0].2, json!({"pane_id": "w1:p2", "workspace_id": "w1"}));
        // Hooks get Herdr's whole envelope.
        assert_eq!(
            hev::envelope("pane.closed", out[0].2.clone()).to_string(),
            r#"{"event":"pane_closed","data":{"type":"pane_closed","pane_id":"w1:p2","workspace_id":"w1"}}"#
        );
    }

    #[test]
    fn git_space_reads_linked_worktrees() {
        let d = tempfile::tempdir().unwrap();
        let main = d.path().join("proj");
        let gitdir = main.join(".git/worktrees/feat");
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::write(gitdir.join("commondir"), "../..\n").unwrap();
        let wt = d.path().join("proj-feat");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();
        let v = git_space(&wt.to_string_lossy()).unwrap();
        assert_eq!(v["is_linked_worktree"], true, "{v}");
        assert_eq!(v["repo_name"], "proj", "{v}");
        assert_eq!(v["checkout_path"], wt.to_string_lossy().to_string(), "{v}");
        let canon = main.canonicalize().unwrap();
        assert_eq!(v["repo_root"], canon.to_string_lossy().to_string(), "{v}");
        assert_eq!(
            v["repo_key"],
            canon.join(".git").to_string_lossy().to_string(),
            "{v}"
        );
        assert!(git_space(&d.path().to_string_lossy()).is_none());
    }

    #[tokio::test]
    async fn popup_close_link_handlers_and_ui_state() {
        let (srv, _d) = server();
        let e = call(&srv, &user(), "popup.close", &json!({}))
            .await
            .unwrap_err();
        assert_eq!(e.code, "popup_not_open");
        let pane = Caller::user(Ctx {
            pane_scope: Some("P".into()),
            ..user().ctx
        });
        let e = call(&srv, &pane, "popup.close", &json!({}))
            .await
            .unwrap_err();
        assert_eq!(e.code, "permission_denied");
        let ctx = user().ctx;
        let r = api::dispatch(&srv, &ctx, "plugin.surface.close", &json!({"pane": "nope"}))
            .await
            .unwrap();
        assert!(r["closed"].is_null(), "already gone is fine");
        srv.with_core(|c| c.model.panes.push(model_pane("PL", "w1:p9", "user")));
        let e = api::dispatch(&srv, &ctx, "plugin.surface.close", &json!({"pane": "PL"}))
            .await
            .unwrap_err();
        assert_eq!(
            e.data.kind, "invalid_params",
            "ordinary panes close with pane.close"
        );
        let e = api::dispatch(&srv, &ctx, "plugin.link.open", &json!({"url": "x"}))
            .await
            .unwrap_err();
        assert_eq!(e.data.kind, "invalid_params");
        let e = api::dispatch(
            &srv,
            &ctx,
            "plugin.link.open",
            &json!({"plugin": "acme.none", "handler": "h", "url": "x"}),
        )
        .await
        .unwrap_err();
        assert_eq!(e.data.kind, "not_found");
        let h = api::dispatch(&srv, &ctx, "plugin.link_handler.list", &json!({}))
            .await
            .unwrap();
        assert!(h["handlers"].is_array());
        // The window title a plugin sets is what clients read on attach.
        call(
            &srv,
            &user(),
            "client.window_title.set",
            &json!({"title": "deploy"}),
        )
        .await
        .unwrap();
        let st = api::dispatch(&srv, &ctx, "compat.ui.state", &json!({}))
            .await
            .unwrap();
        assert_eq!(st["window_title"], "deploy");
        assert!(st["popup"].is_null());
        call(&srv, &user(), "client.window_title.clear", &json!({}))
            .await
            .unwrap();
        let st = api::dispatch(&srv, &ctx, "compat.ui.state", &json!({}))
            .await
            .unwrap();
        assert!(st["window_title"].is_null());
    }

    #[test]
    fn herdr_paths_are_never_used() {
        let home = crate::paths::home();
        assert!(is_herdr_owned(&home.join(".config/herdr/herdr.sock")));
        assert!(is_herdr_owned(
            &home.join(".config/herdr/sessions/x/herdr.sock")
        ));
        assert!(!is_herdr_owned(Path::new(
            "/tmp/vibeke-1/default/herdr-compat/herdr.sock"
        )));
    }
}
