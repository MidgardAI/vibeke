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
//!   re-check the grant on every request, are re-issued after a server restart for live
//!   invocations, and outlive the invocation's process only for the manifest's long-running
//!   entrypoints (`[[startup]]` process groups, `[[panes]]`).
//! * **Method mapping**: Herdr methods are translated onto the native API (`api::dispatch`, so
//!   native authorization and events apply) and results are projected to Herdr shapes with
//!   Herdr-style ids (Vibeke handles); slice-2 methods live in [`ext`]. Known-but-unimplemented
//!   baseline methods return an explicit `unsupported` error; `method_not_found` is reserved
//!   for unknown methods.
//! * **Plugins**: `plugin.action.list/run` (native) and `plugin.action.invoke` (compat) run
//!   argv actions asynchronously with log records (output tailed from files so it survives a
//!   server restart, credentials redacted with `vk-redact`); `[[events]]` hooks fire on
//!   projected Herdr events; `[[startup]]` runs once per server activation. Nothing runs without
//!   a valid `herdr_legacy` grant (`vibeke plugin trust <id> --legacy`). Invocations and
//!   mutating broker calls are audited as metadata-only events (`plugin.invocation_started`,
//!   `plugin.invocation_finished`, `plugin.api_call` with `actor.kind = plugin`).

mod brokers;
mod ext;

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
    ("compat.status", false),
];

/// Log records kept per server (oldest dropped first) and bytes kept per stream.
const MAX_LOGS: usize = 100;
const MAX_STREAM: usize = 64 * 1024;

// ---- per-server state -------------------------------------------------------------------------

#[derive(Default)]
struct State {
    logs: Mutex<VecDeque<Value>>,
    next: AtomicU64,
    /// Live broker bindings by socket path.
    bindings: Mutex<HashMap<PathBuf, brokers::Live>>,
    /// Reported metadata (`pane.report_metadata`, `workspace.report_metadata`) by Vibeke id,
    /// and the plugin-set window title (`client.window_title.set`).
    meta: Mutex<ext::Meta>,
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
    herdr::session_socket(&herdr_root(server), &server.opts.session)
}

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
    if is_herdr_owned(&path) {
        return Err(std::io::Error::other(
            "refusing to bind inside Herdr's config directory",
        ));
    }
    let root = herdr_root(server);
    private_dir(&root)?;
    if let Some(dir) = path.parent()
        && dir != root
    {
        private_dir(&root.join("sessions"))?;
        private_dir(dir)?;
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
}

impl Caller {
    /// A user-level caller (CLI shim over the native socket, tests).
    pub fn user(ctx: Ctx) -> Self {
        Caller {
            ctx,
            plugin: None,
            default_pane: None,
            invocation: None,
        }
    }
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
            | "pane.list"
            | "pane.get"
            | "pane.current"
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
            | "events.subscribe"
            | "events.wait"
            | "plugin.list"
            | "plugin.action.list"
            | "plugin.log.list"
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

/// The broker's grant must still be valid: registered, trusted with the same digest, enabled.
fn check_broker(caller: &Caller) -> Result<(), WireError> {
    let Some((id, digest)) = &caller.plugin else {
        return Ok(());
    };
    let reg = Registry::load(&plugin_dirs())
        .map_err(|e| WireError::new("internal_error", e.to_string()))?;
    let ok = reg.get(id).ok().is_some_and(|e| {
        matches!(registry::entry_status(e), (Status::Active, _))
            && e.trust
                .as_ref()
                .is_some_and(|g| &g.manifest_sha256 == digest)
    });
    if ok {
        Ok(())
    } else {
        Err(WireError::new(
            "permission_denied",
            format!("plugin {id} is no longer trusted or enabled"),
        ))
    }
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
    let req = match wire::parse_request(&buf) {
        Ok(r) => r,
        Err((id, e)) => {
            let _ = write_line(&mut wr, &wire::err_line(&id, &e)).await;
            return;
        }
    };
    if let Err(e) = check_broker(&caller) {
        let _ = write_line(&mut wr, &wire::err_line(&req.id, &e)).await;
        return;
    }
    if req.method == "events.subscribe" {
        let subs = match hev::parse_subscriptions(&req.params) {
            Ok(s) => s,
            Err((code, msg)) => {
                let _ = write_line(
                    &mut wr,
                    &wire::err_line(&req.id, &WireError::new(code, msg)),
                )
                .await;
                return;
            }
        };
        let mut rx = server.events.subscribe();
        let mut proj = seeded_projector(&server);
        let ack = typed("subscription_started", json!({}));
        if write_line(&mut wr, &wire::ok_line(&req.id, ack))
            .await
            .is_err()
        {
            return;
        }
        loop {
            let ev = match rx.recv().await {
                Ok(e) => e,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return,
            };
            for (name, pane, data) in project_event(&server, &mut proj, &ev) {
                if !subs.iter().any(|s| hev::matches(s, name, pane.as_deref())) {
                    continue;
                }
                if check_broker(&caller).is_err() {
                    return;
                }
                if write_line(&mut wr, &wire::event_line(name, data))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
    }
    let r = call(&server, &caller, &req.method, &req.params).await;
    audit_call(
        &server,
        &caller,
        &req.method,
        r.as_ref().map(|_| ()).map_err(|e| e.code.as_str()),
    );
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
    focus: ClientFocus,
    meta: ext::Meta,
}

fn snap(server: &Server) -> Snap {
    let focus = crate::notify::recent_client(server)
        .map(|c| server.client_focus(&c))
        .unwrap_or_default();
    let meta = state(server).meta.lock().unwrap().clone();
    server.with_core(|c| {
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
            panes: c.model.panes.clone(),
            pane_run,
            blocked,
            focus,
            meta,
        }
    })
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
    fn ws_json(&self, w: &Workspace) -> Value {
        let idx = self.ws.iter().position(|x| x.id == w.id).unwrap_or(0);
        let tabs: Vec<&Tab> = self.tabs.iter().filter(|t| t.workspace == w.id).collect();
        let active = tabs
            .iter()
            .find(|t| self.focus.tab.as_deref() == Some(&t.id))
            .or(tabs.first())
            .map(|t| t.handle.clone());
        let statuses: Vec<&str> = self
            .panes
            .iter()
            .filter(|p| p.workspace == w.id)
            .filter_map(|p| self.pane_status(&p.id))
            .collect();
        let mut v = json!({
            "workspace_id": w.handle,
            "number": idx + 1,
            "label": w.display_name(),
            "focused": self.focus.workspace.as_deref() == Some(&w.id),
            "pane_count": self.panes.iter().filter(|p| p.workspace == w.id).count(),
            "tab_count": tabs.len(),
            "active_tab_id": active,
            "agent_status": status::most_urgent(statuses),
            "cwd": w.root_path,
        });
        if let Some(m) = self.meta.workspaces.get(&w.id) {
            v["metadata"] = json!(m);
        }
        v
    }
    fn tab_json(&self, t: &Tab) -> Value {
        json!({
            "tab_id": t.handle,
            "workspace_id": self.ws_handle(&t.workspace),
            "number": t.number,
            "label": t.title.clone().unwrap_or_default(),
            "focused": self.focus.tab.as_deref() == Some(&t.id),
            "pane_count": self.panes.iter().filter(|p| p.tab == t.id).count(),
            "focused_pane_id": t.focused_pane.as_deref().and_then(|p| self.pane(p)).map(|p| p.handle.clone()),
        })
    }
    fn pane_json(&self, server: &Server, p: &Pane) -> Value {
        let run = self.pane_run.get(&p.id);
        let cwd = server.pane_cwd(&p.id).or_else(|| p.cwd.clone());
        let rev = server.pane_rt(&p.id).map(|r| r.rev()).unwrap_or(0);
        let session = run.and_then(|r| {
            r.harness_session_id
                .as_ref()
                .map(|v| ("id", v.clone()))
                .or_else(|| r.transcript_path.as_ref().map(|v| ("path", v.clone())))
                .map(|(kind, value)| {
                    json!({"source": "vibeke", "agent": status::herdr_agent_name(&r.harness), "kind": kind, "value": value})
                })
        });
        let mut v = json!({
            "pane_id": p.handle,
            "terminal_id": p.handle,
            "workspace_id": self.ws_handle(&p.workspace),
            "tab_id": self.tab_handle(&p.tab),
            "focused": self.focus.pane.as_deref() == Some(&p.id),
            "cwd": cwd,
            "foreground_cwd": cwd,
            "agent": run.map(|r| status::herdr_agent_name(&r.harness).to_string()),
            "agent_status": run.map(|r| self.run_status(r)),
            "agent_session": session,
            "revision": rev,
            "scroll": 0,
        });
        if let Some(t) = &p.title {
            v["label"] = json!(t);
        }
        if let Some(m) = self.meta.panes.get(&p.id) {
            v["metadata"] = json!(m);
        }
        v
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
    fn focused(&self) -> (Option<String>, Option<String>, Option<String>) {
        (
            self.focus
                .workspace
                .as_deref()
                .and_then(|x| self.ws_handle(x)),
            self.focus.tab.as_deref().and_then(|x| self.tab_handle(x)),
            self.focus
                .pane
                .as_deref()
                .and_then(|x| self.pane(x))
                .map(|p| p.handle.clone()),
        )
    }
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

/// Herdr events for one Vibeke event: `(dotted name, Herdr pane id, data)`.
fn project_event(
    server: &Server,
    proj: &mut Projector,
    ev: &vk_store::Event,
) -> Vec<(&'static str, Option<String>, Value)> {
    let subj = &ev.subject;
    let sn = snap(server);
    let sid = |k: &str| subj.get(k).and_then(Value::as_str);
    // Subject ids are ULIDs; translate to Herdr ids (handles), falling back to the handle the
    // subject carries for objects already gone.
    let pane = sid("pane")
        .and_then(|p| sn.pane(p))
        .map(|p| p.handle.clone())
        .or_else(|| sid("pane_handle").map(str::to_string));
    let pane_obj = sid("pane").and_then(|p| sn.pane(p));
    let tab = sid("tab")
        .and_then(|t| sn.tab_handle(t))
        .or_else(|| pane_obj.and_then(|p| sn.tab_handle(&p.tab)));
    let ws = sid("workspace")
        .and_then(|w| sn.ws_handle(w))
        .or_else(|| pane_obj.and_then(|p| sn.ws_handle(&p.workspace)));
    let status_after = sid("pane").and_then(|p| sn.pane_status(p));
    let input = hev::Input {
        kind: &ev.kind,
        workspace: ws.as_deref(),
        tab: tab.as_deref(),
        pane: pane.as_deref(),
        status_after,
    };
    let names = proj.project(&input);
    names
        .into_iter()
        .map(|name| {
            let mut d = Map::new();
            if let Some(w) = &ws {
                d.insert("workspace_id".into(), json!(w));
            }
            if let Some(t) = &tab {
                d.insert("tab_id".into(), json!(t));
            }
            if let Some(p) = &pane {
                d.insert("pane_id".into(), json!(p));
            }
            if name.starts_with("workspace.")
                && let Some(w) = sid("workspace").and_then(|w| sn.ws(w))
            {
                d.insert("workspace".into(), sn.ws_json(w));
            }
            if name.starts_with("tab.")
                && let Some(t) = sid("tab").and_then(|t| sn.tab(t))
            {
                d.insert("tab".into(), sn.tab_json(t));
            }
            if name.starts_with("pane.")
                && let Some(p) = pane_obj
            {
                d.insert("pane".into(), sn.pane_json(server, p));
            }
            if name == "pane.agent_status_changed" || name == "pane.agent_detected" {
                d.insert("agent_status".into(), json!(status_after));
                d.insert(
                    "agent".into(),
                    json!(
                        pane_obj
                            .and_then(|p| sn.pane_run.get(&p.id))
                            .map(|r| status::herdr_agent_name(&r.harness))
                    ),
                );
            }
            match name {
                "worktree.created" | "worktree.opened" | "worktree.removed" => {
                    let mut wt = ev.data.clone();
                    if let Some(o) = wt.as_object_mut() {
                        o.retain(|k, _| matches!(k.as_str(), "path" | "branch" | "repo_root"));
                    }
                    d.insert("worktree".into(), wt);
                    d.insert("data".into(), ev.data.clone());
                }
                "pane.moved" => {
                    for k in [
                        "from_tab_id",
                        "to_tab_id",
                        "from_workspace_id",
                        "to_workspace_id",
                    ] {
                        if let Some(v) = ev.data.get(k) {
                            d.insert(k.into(), v.clone());
                        }
                    }
                }
                "tab.moved" => {
                    if let Some(i) = ev.data.get("index") {
                        d.insert("insert_index".into(), i.clone());
                    }
                }
                "layout.updated" => {
                    if let Some(t) = sid("tab").and_then(|t| sn.tab(t)) {
                        d.insert("layout".into(), ext::tab_snapshot(&sn, t));
                    }
                }
                "pane.output_matched" => {
                    for k in ["matched", "revision"] {
                        if let Some(v) = ev.data.get(k) {
                            d.insert(k.into(), v.clone());
                        }
                    }
                }
                _ => {}
            }
            (name, pane.clone(), Value::Object(d))
        })
        .collect()
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

/// The pane a request targets: `pane_id`, else the invocation's pane, else `@current` for
/// pane-scoped callers, else the focused pane of the most recent client.
fn pane_target(caller: &Caller, sn: &Snap, p: &Value) -> Result<String, WireError> {
    if let Some(id) = sp(p, "pane_id") {
        return sn
            .pane(id)
            .map(|x| x.id.clone())
            .ok_or_else(|| WireError::new("pane_not_found", format!("pane not found: {id}")));
    }
    if let Some(d) = &caller.default_pane
        && let Some(x) = sn.pane(d)
    {
        return Ok(x.id.clone());
    }
    if let Some(s) = &caller.ctx.pane_scope {
        return Ok(s.clone());
    }
    sn.focus
        .pane
        .clone()
        .ok_or_else(|| WireError::new("invalid_params", "pane_id is required (no focused pane)"))
}

fn ws_target(sn: &Snap, p: &Value) -> Result<Workspace, WireError> {
    let id = req_s(p, "workspace_id")?;
    sn.ws(id)
        .cloned()
        .ok_or_else(|| WireError::new("workspace_not_found", format!("workspace not found: {id}")))
}

fn tab_target(sn: &Snap, p: &Value) -> Result<Tab, WireError> {
    let id = req_s(p, "tab_id")?;
    sn.tab(id)
        .cloned()
        .ok_or_else(|| WireError::new("tab_not_found", format!("tab not found: {id}")))
}

fn run_target(sn: &Snap, p: &Value) -> Result<AgentRun, WireError> {
    let t = req_s(p, "target").or_else(|_| req_s(p, "agent_id"))?;
    sn.pane_run
        .values()
        .find(|r| r.handle == t || r.id == t || r.name.as_deref() == Some(t))
        .or_else(|| sn.pane(t).and_then(|x| sn.pane_run.get(&x.id)))
        .cloned()
        .ok_or_else(|| WireError::new("agent_not_found", format!("agent not found: {t}")))
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
    let sn = snap(server);
    match method {
        "ping" => Ok(typed(
            "pong",
            json!({"version": herdr::BASELINE_VERSION, "emulator": "vibeke", "vibeke_version": vk_proto::VERSION, "compatibility": "partial"}),
        )),
        "api.schema" => Ok(typed(
            "api_schema",
            json!({
                "version": herdr::BASELINE_VERSION,
                "methods": inventory::ENTRIES.iter().filter(|e| e.kind == inventory::Kind::Method).map(|e| json!({"name": e.name, "status": e.status.as_str()})).collect::<Vec<_>>(),
                "events": hev::BASELINE_EVENTS,
            }),
        )),
        "session.snapshot" => {
            let (fw, ft, fp) = sn.focused();
            Ok(typed(
                "session_snapshot",
                json!({
                    "version": herdr::BASELINE_VERSION,
                    "protocol": herdr::BASELINE_VERSION,
                    "workspaces": sn.ws.iter().map(|w| sn.ws_json(w)).collect::<Vec<_>>(),
                    "tabs": sn.tabs.iter().map(|t| sn.tab_json(t)).collect::<Vec<_>>(),
                    "panes": sn.panes.iter().map(|x| sn.pane_json(server, x)).collect::<Vec<_>>(),
                    "agents": sn.pane_run.values().map(|r| sn.agent_json(server, r)).collect::<Vec<_>>(),
                    "layouts": sn.tabs.iter().map(|t| ext::tab_snapshot(&sn, t)).collect::<Vec<_>>(),
                    "focused_workspace_id": fw,
                    "focused_tab_id": ft,
                    "focused_pane_id": fp,
                }),
            ))
        }
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
            let mut np = json!({"focus": p.get("focus").and_then(Value::as_bool).unwrap_or(false)});
            if let Some(c) = sp(p, "cwd") {
                np["cwd"] = json!(c);
            }
            if let Some(l) = sp(p, "label") {
                np["name"] = json!(l);
            }
            let r = native(server, caller, "workspace.create", np).await?;
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
            let mut np = json!({"workspace": w.id, "focus": p.get("focus").and_then(Value::as_bool).unwrap_or(false)});
            if let Some(l) = sp(p, "label") {
                np["title"] = json!(l);
            }
            np["cwd"] = json!(sp(p, "cwd").unwrap_or(&w.root_path));
            let r = native(server, caller, "tab.create", np).await?;
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
        "pane.get" | "pane.current" => {
            let id = if method == "pane.current" {
                pane_target(caller, &sn, &json!({}))?
            } else {
                pane_target(caller, &sn, p)?
            };
            let x = sn
                .pane(&id)
                .ok_or_else(|| WireError::new("pane_not_found", id.clone()))?;
            Ok(typed("pane_info", json!({"pane": sn.pane_json(server, x)})))
        }
        "pane.read" => {
            let id = pane_target(caller, &sn, p)?;
            let source = sp(p, "source").unwrap_or("recent");
            if !matches!(
                source,
                "visible" | "recent" | "recent_unwrapped" | "detection"
            ) {
                return Err(WireError::new(
                    "invalid_params",
                    format!("unknown source `{source}`"),
                ));
            }
            let lines = p.get("lines").and_then(Value::as_u64).unwrap_or(50);
            let r = native(
                server,
                caller,
                "pane.read",
                json!({"pane": id, "source": source, "lines": lines}),
            )
            .await?;
            Ok(typed(
                "pane_read",
                json!({"read": {"text": r["text"], "truncated": false, "revision": r["revision"]}}),
            ))
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
                .ok_or_else(|| WireError::new("pane_not_found", id.clone()))?;
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
                .ok_or_else(|| WireError::new("pane_not_found", id.clone()))?;
            Ok(typed("pane_info", json!({"pane": sn.pane_json(server, x)})))
        }
        "pane.close" => {
            let id = pane_target(caller, &sn, p)?;
            native(server, caller, "pane.close", json!({"pane": id})).await?;
            Ok(ok())
        }
        "pane.split" => {
            let id = pane_target(caller, &sn, p)?;
            let mut np = json!({"pane": id, "direction": sp(p, "direction").unwrap_or("right"), "focus": p.get("focus").and_then(Value::as_bool).unwrap_or(false)});
            if let Some(c) = sp(p, "cwd") {
                np["cwd"] = json!(c);
            }
            let r = native(server, caller, "pane.split", np).await?;
            let sn = snap(server);
            let new = r["pane"]["id"].as_str().unwrap_or_default();
            Ok(typed(
                "pane_info",
                json!({"pane": sn.pane(new).map(|x| sn.pane_json(server, x))}),
            ))
        }
        "pane.wait_for_output" => {
            let id = pane_target(caller, &sn, p)?;
            let mut np = json!({"pane": id, "timeout_ms": p.get("timeout_ms").and_then(Value::as_u64).unwrap_or(30_000)});
            match (
                sp(p, "regex"),
                sp(p, "match").or(sp(p, "pattern")).or(sp(p, "text")),
            ) {
                (Some(r), _) => np["regex"] = json!(r),
                (None, Some(m)) => np["match"] = json!(m),
                _ => {
                    return Err(WireError::new(
                        "invalid_params",
                        "match or regex is required",
                    ));
                }
            }
            let r = native(server, caller, "pane.wait_output", np).await?;
            // Registered matchers feed the `pane.output_matched` event (07 §8.3).
            if let Some(x) = sn.pane(&id) {
                let mut c = server.core.lock().unwrap();
                let mut tx = crate::core::Tx::new();
                tx.event(
                    "pane.output_matched",
                    crate::core::subject_pane(x),
                    json!({"matched": r["matched"], "revision": r["revision"]}),
                );
                let _ = server.commit(&mut c, tx);
            }
            Ok(typed(
                "pane_output_matched",
                json!({"pane_id": sn.pane(&id).map(|x| x.handle.clone()), "matched": r["matched"], "revision": r["revision"]}),
            ))
        }
        "pane.report_agent" | "pane.report_agent_session" => {
            let mut np = p.clone();
            if let Some(id) = sp(p, "pane_id") {
                let x = sn
                    .pane(id)
                    .ok_or_else(|| WireError::new("pane_not_found", id.to_string()))?;
                np["pane_id"] = json!(x.id);
            } else if let Some(d) = &caller.default_pane {
                np["pane_id"] = json!(d);
            }
            native(server, caller, method, np).await?;
            Ok(ok())
        }
        "pane.resize" => {
            let id = pane_target(caller, &sn, p)?;
            let mut np = json!({"pane": id, "direction": req_s(p, "direction")?});
            if let Some(a) = p.get("amount").or(p.get("percent")) {
                np["percent"] = a.clone();
            }
            native(server, caller, "pane.resize", np).await?;
            Ok(ok())
        }
        "pane.zoom" => {
            let id = pane_target(caller, &sn, p)?;
            let mut np = json!({"pane": id});
            if let Some(z) = p.get("zoomed") {
                np["zoomed"] = z.clone();
            }
            native(server, caller, "pane.zoom", np).await?;
            Ok(ok())
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
            let rr = native(
                server,
                caller,
                "pane.read",
                json!({"pane": r.pane, "source": sp(p, "source").unwrap_or("recent"), "lines": p.get("lines").and_then(Value::as_u64).unwrap_or(50)}),
            )
            .await?;
            Ok(typed(
                "pane_read",
                json!({"read": {"text": rr["text"], "truncated": false, "revision": rr["revision"]}}),
            ))
        }
        // ---- worktrees, notifications, server --------------------------------------------
        "worktree.list" => {
            let r = native(
                server,
                caller,
                "worktree.list",
                json!({"cwd": req_s(p, "cwd")?}),
            )
            .await?;
            Ok(typed("worktree_list", json!({"worktrees": r["worktrees"]})))
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
            let mut np = json!({"title": req_s(p, "title")?});
            if let Some(b) = sp(p, "body").or(sp(p, "message")) {
                np["body"] = json!(b);
            }
            native(server, caller, "notification.send", np).await?;
            Ok(ok())
        }
        "server.reload_config" => Ok(ok()),
        "events.wait" => events_wait(server, p).await,
        "events.subscribe" => Err(WireError::new(
            "invalid_request",
            "events.subscribe streams on its own socket connection",
        )),
        // ---- plugins ---------------------------------------------------------------------
        "plugin.list" => Ok(typed("plugin_list", json!({"plugins": plugin_list()}))),
        "plugin.action.list" => Ok(typed(
            "plugin_action_list",
            json!({"actions": action_list(sp(p, "plugin_id"))?}),
        )),
        "plugin.action.invoke" => {
            let (plugin, action) = match (sp(p, "plugin_id"), sp(p, "action_id"), sp(p, "action")) {
                (Some(pl), Some(a), _) => (pl.to_string(), a.to_string()),
                (_, _, Some(q)) => split_qualified(q)?,
                _ => {
                    return Err(WireError::new(
                        "invalid_params",
                        "plugin_id and action_id (or action) are required",
                    ));
                }
            };
            let ctx = InvokeContext::from_params(&sn, p, caller);
            let log = invoke_action(
                server,
                caller.ctx.pane_scope.as_deref(),
                &plugin,
                &action,
                ctx,
                "api",
            )?;
            Ok(typed("plugin_action_started", json!({"log": log})))
        }
        "plugin.log.list" => Ok(typed(
            "plugin_log_list",
            json!({"logs": logs(server, sp(p, "plugin_id"), p.get("limit").and_then(Value::as_u64))}),
        )),
        "server.stop" => Err(WireError::new(
            "unsupported",
            "server.stop is refused on the Herdr compatibility endpoint; use `vibeke server stop`",
        )),
        m if ext::METHODS.contains(&m) => ext::call(server, caller, &sn, m, p).await,
        other => match inventory::method_status(other) {
            Some(_) => Err(WireError::new(
                "unsupported",
                format!(
                    "{other} is part of the Herdr {} surface but not implemented by Vibeke yet (see docs/herdr-compat-inventory.md)",
                    herdr::BASELINE_VERSION
                ),
            )),
            None => Err(WireError::new(
                "method_not_found",
                format!("unknown method: {other}"),
            )),
        },
    }
}

fn keys(p: &Value) -> Result<Value, WireError> {
    match p.get("keys") {
        Some(Value::Array(a)) => Ok(Value::Array(a.clone())),
        Some(Value::String(s)) => Ok(json!(s.split_whitespace().collect::<Vec<_>>())),
        _ => Err(WireError::new("invalid_params", "keys is required")),
    }
}

async fn events_wait(server: &Arc<Server>, p: &Value) -> Result<Value, WireError> {
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
        for (name, pane, data) in project_event(server, &mut proj, &ev) {
            if subs.iter().any(|s| hev::matches(s, name, pane.as_deref())) {
                return Ok(typed(
                    "event",
                    json!({"event": hev::wire_name(name), "data": data}),
                ));
            }
        }
    }
}

// ---- plugins: listing, invocation, logs -------------------------------------------------------

fn plugin_list() -> Vec<Value> {
    let dirs = plugin_dirs();
    let Ok(reg) = Registry::load(&dirs) else {
        return vec![];
    };
    reg.plugins
        .values()
        .map(|e| {
            let (st, m) = registry::entry_status(e);
            json!({
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
            format!("plugin not found: {id}"),
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

/// Invocation context (`PluginInvocationContext` subset).
#[derive(Debug, Clone, Default)]
pub struct InvokeContext {
    pub workspace: Option<String>,
    pub tab: Option<String>,
    pub pane: Option<String>,
}

impl InvokeContext {
    /// Explicit `workspace_id/tab_id/pane_id` (Herdr ids or Vibeke handles), else the caller's
    /// pane, else the focus of the most recently active client.
    fn from_params(sn: &Snap, p: &Value, caller: &Caller) -> Self {
        let ctx = p.get("context").unwrap_or(p);
        let pane = sp(ctx, "pane_id")
            .or(sp(ctx, "pane"))
            .and_then(|x| sn.pane(x))
            .or_else(|| caller.default_pane.as_deref().and_then(|x| sn.pane(x)))
            .or_else(|| caller.ctx.pane_scope.as_deref().and_then(|x| sn.pane(x)))
            .or_else(|| {
                if sp(ctx, "workspace_id").or(sp(ctx, "workspace")).is_some() {
                    None
                } else {
                    sn.focus.pane.as_deref().and_then(|x| sn.pane(x))
                }
            });
        let ws = sp(ctx, "workspace_id")
            .or(sp(ctx, "workspace"))
            .and_then(|x| sn.ws(x))
            .map(|w| w.handle.clone())
            .or_else(|| pane.and_then(|x| sn.ws_handle(&x.workspace)));
        let tab = sp(ctx, "tab_id")
            .or(sp(ctx, "tab"))
            .and_then(|x| sn.tab(x))
            .map(|t| t.handle.clone())
            .or_else(|| pane.and_then(|x| sn.tab_handle(&x.tab)));
        InvokeContext {
            workspace: ws,
            tab,
            pane: pane.map(|x| x.handle.clone()),
        }
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
        .map_err(|_| WireError::new("plugin_not_found", format!("plugin not found: {plugin}")))?
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
                }
            }
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
        let mut logs = st.logs.lock().unwrap();
        logs.push_back(rec);
        while logs.len() > MAX_LOGS {
            logs.pop_front();
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

/// Keep the last [`MAX_STREAM`] bytes of a stream.
fn tail_keep(buf: &mut Vec<u8>) {
    if buf.len() > MAX_STREAM {
        buf.drain(..buf.len() - MAX_STREAM);
    }
}

/// The text of a stream tail as stored in a log record: lossy UTF-8 with credentials redacted
/// (09 §6 "Logs and audit records redact credentials").
fn redacted(buf: &[u8]) -> String {
    let text = String::from_utf8_lossy(buf);
    vk_redact::redact(&text).into_owned()
}

/// Tails an invocation's stdout/stderr files into its log record.
struct Tail {
    files: [(Option<PathBuf>, u64, Vec<u8>); 2],
}

impl Tail {
    fn new(out: Option<PathBuf>, err: Option<PathBuf>) -> Self {
        Tail {
            files: [(out, 0, Vec::new()), (err, 0, Vec::new())],
        }
    }

    /// Read what was appended since the last poll; update the record when anything changed.
    fn poll(&mut self, server: &Server, log_id: &str) {
        use std::io::{Read, Seek, SeekFrom};
        let mut changed = false;
        for (path, off, buf) in self.files.iter_mut() {
            let Some(p) = path else { continue };
            let Ok(mut f) = std::fs::File::open(&*p) else {
                continue;
            };
            if f.seek(SeekFrom::Start(*off)).is_err() {
                continue;
            }
            let mut chunk = Vec::new();
            if f.read_to_end(&mut chunk).is_ok() && !chunk.is_empty() {
                *off += chunk.len() as u64;
                buf.extend_from_slice(&chunk);
                tail_keep(buf);
                changed = true;
            }
        }
        if changed {
            let (o, e) = (redacted(&self.files[0].2), redacted(&self.files[1].2));
            update_log(server, log_id, |l| {
                l.insert("stdout".into(), json!(o));
                l.insert("stderr".into(), json!(e));
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
        l.insert(
            "status".into(),
            json!(if ok { "completed" } else { "failed" }),
        );
        l.insert("exit_code".into(), json!(code));
        l.insert("finished_at".into(), json!(now_ms()));
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
            "status": if ok { "completed" } else { "failed" },
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
        let mut tail = Tail::new(out, err);
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
) -> Value {
    let st = state(server);
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
    let context = json!({
        "source": sp_.source,
        "correlation_id": log_id,
        "plugin_id": entry.id,
        "action_id": sp_.action,
        "event": sp_.event.as_ref().map(|e| e.0.clone()),
        "workspace_id": sp_.ctx.workspace,
        "tab_id": sp_.ctx.tab,
        "pane_id": sp_.ctx.pane,
        "plugin_version": m.version,
    });
    let mut rec = json!({
        "log_id": log_id,
        "plugin_id": entry.id,
        "action_id": sp_.action,
        "event": sp_.event.as_ref().map(|e| e.0.clone()),
        "entrypoint_id": sp_.entrypoint,
        "source": sp_.source,
        "status": "running",
        "started_at": now_ms(),
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
    // Output goes to files, not pipes: a long-lived invocation keeps writing across a server
    // restart (a pipe would break and SIGPIPE it).
    let out_dir = compat_root(server).join("out");
    let (out_path, err_path) = (
        out_dir.join(format!("{log_id}.out")),
        out_dir.join(format!("{log_id}.err")),
    );
    let files =
        private_dir(&out_dir).and_then(|_| Ok((out_file(&out_path)?, out_file(&err_path)?)));
    let (out_f, err_f) = match files {
        Ok(f) => f,
        Err(e) => return fail(rec, format!("output: {e}")),
    };
    // Private broker bound to this invocation's grant.
    let broker = match brokers::new_path(server) {
        Ok(p) => p,
        Err(e) => return fail(rec, format!("broker: {e}")),
    };
    let binding = brokers::Binding {
        path: broker.clone(),
        plugin_id: entry.id.clone(),
        digest,
        default_pane: sp_.ctx.pane.clone(),
        entrypoint: sp_.entrypoint.clone(),
        source: sp_.source.to_string(),
        log_id: Some(log_id.clone()),
        life: brokers::Life::Pending,
        created_at_ms: now_ms(),
        stdout: Some(out_path.clone()),
        stderr: Some(err_path.clone()),
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
        workspace_id: sp_.ctx.workspace.clone(),
        tab_id: sp_.ctx.tab.clone(),
        pane_id: sp_.ctx.pane.clone(),
        action_id: sp_.action.clone(),
        event: sp_.event.clone(),
        entrypoint_id: sp_.entrypoint.clone(),
        clicked_url: None,
        link_handler_id: None,
    };
    let env = launch::runtime_env(&inv, std::env::vars());
    let argv = launch::resolve_argv(&entry.root, command);
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
            tokio::spawn(async move {
                let mut tail = Tail::new(Some(out_path), Some(err_path));
                let status = loop {
                    tokio::select! {
                        s = child.wait() => break s,
                        _ = tokio::time::sleep(Duration::from_millis(150)) => tail.poll(&srv, &id),
                    }
                };
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
                if long_lived {
                    while brokers::get(&srv, &broker).is_some() {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                    tail.remove();
                }
            });
            rec
        }
    }
}

// ---- hooks ------------------------------------------------------------------------------------

fn run_startup_hooks(server: &Arc<Server>) {
    let Ok(reg) = Registry::load(&plugin_dirs()) else {
        return;
    };
    let pf = herdr::current_platform();
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
                    event: None,
                    entrypoint: Some(format!("startup[{i}]")),
                    ctx: InvokeContext::default(),
                    long_lived: true,
                },
            );
        }
    }
}

/// Active plugins, re-read when `plugins.json` changes and at most every 2 s otherwise (so a
/// manifest edit takes effect quickly without parsing manifests on every event).
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
            for (entry, m) in &active {
                for (i, h) in m.hooks_for(name, pf).into_iter().enumerate() {
                    let ctx = InvokeContext {
                        workspace: data
                            .get("workspace_id")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        tab: data
                            .get("tab_id")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        pane: data
                            .get("pane_id")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    };
                    spawn_invocation(
                        &server,
                        entry,
                        m,
                        &h.command,
                        Spawn {
                            source: "event",
                            action: None,
                            event: Some((name.to_string(), data.clone())),
                            entrypoint: Some(
                                h.id.clone().unwrap_or_else(|| format!("events[{i}]")),
                            ),
                            ctx,
                            long_lived: false,
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
        "method_not_found" => ErrorKind::MethodNotFound,
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
            .map(|a| json!({"actions": a}))
            .map_err(wire_to_rpc),
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
            let sn = snap(server);
            let ictx = InvokeContext::from_params(&sn, p, &Caller::user(ctx.clone()));
            invoke_action(
                server,
                ctx.pane_scope.as_deref(),
                &plugin,
                &action,
                ictx,
                "cli",
            )
            .map(|log| json!({"log": log}))
        })()
        .map_err(wire_to_rpc),
        "plugin.log.list" => {
            Ok(json!({"logs": logs(server, s("plugin"), p.get("limit").and_then(Value::as_u64))}))
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
            // plugin identity: the grant is re-checked here, on the destination (09 §6).
            let mut caller = Caller::user(ctx.clone());
            if let Some(id) = s("as_plugin") {
                if ctx.pane_scope.is_some() {
                    return Some(Err(err(
                        ErrorKind::PermissionDenied,
                        "a pane cannot act as a plugin",
                    )));
                }
                let digest = Registry::load(&plugin_dirs())
                    .ok()
                    .and_then(|r| r.get(id).ok().cloned())
                    .filter(|e| matches!(registry::entry_status(e), (Status::Active, _)))
                    .and_then(|e| e.trust.map(|g| g.manifest_sha256));
                let Some(digest) = digest else {
                    return Some(Ok(
                        json!({"error": {"code": "permission_denied", "message": format!("plugin {id} is not trusted and enabled")}}),
                    ));
                };
                caller.plugin = Some((id.to_string(), digest));
                caller.ctx.kind = "plugin".into();
            }
            let r = call(server, &caller, m, &params).await;
            audit_call(
                server,
                &caller,
                m,
                r.as_ref().map(|_| ()).map_err(|e| e.code.as_str()),
            );
            Ok(match r {
                Ok(v) => json!({"result": v}),
                Err(e) => json!({"error": {"code": e.code, "message": e.message}}),
            })
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
        assert_eq!(r.unwrap_err().code, "method_not_found");
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
        assert_eq!(v["version"], herdr::BASELINE_VERSION);
        assert!(v["focused_pane_id"].is_null());
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
        assert_eq!(e.code, "invalid_params", "no pane and no focus");
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
        assert_eq!(r["error"]["code"], "method_not_found");
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
        assert_eq!(check_broker(&ghost).unwrap_err().code, "permission_denied");
        assert!(check_broker(&user()).is_ok());
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
            life,
            created_at_ms: 0,
            stdout: None,
            stderr: None,
        };
        let me = std::process::id();
        let list = vec![
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
        assert_eq!(brokers::recover(&srv), (0, 3));
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

    #[test]
    fn plugin_log_tails_are_redacted() {
        let token = format!("ghp_{}", "A".repeat(36));
        let out = redacted(format!("pushing with {token}\n").as_bytes());
        assert!(!out.contains(&token), "{out}");
        assert!(out.contains("pushing with"));
        let mut big = vec![b'x'; MAX_STREAM + 10];
        tail_keep(&mut big);
        assert_eq!(big.len(), MAX_STREAM);
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
