//! Server process: control socket, connection handling, housekeeping, task API.

use crate::api::{self, Ctx, R, err, internal, invalid, not_found, req, s};
use crate::core::{Tx, ulid};
use crate::{Server, render};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use vk_proto::model::Task;
use vk_proto::rpc::{ErrorKind, Request, Response};

/// Longest request line on the control socket (`api.schema` `max_line_bytes`); a longer one
/// ends the connection.
pub const MAX_CONTROL_LINE: usize = 16 * 1024 * 1024;

/// `read_line` that refuses lines longer than `max` bytes (excluding the newline): an error
/// ends the caller's connection instead of buffering without bound. Like `read_line`, partial
/// input stays in `line` when the future is dropped (`select!`), and the limit counts it.
pub(crate) async fn read_line_capped<R: AsyncBufRead + Unpin>(
    rd: &mut R,
    line: &mut String,
    max: usize,
) -> std::io::Result<usize> {
    let room = (max + 1).saturating_sub(line.len()) as u64;
    let n = (&mut *rd).take(room).read_line(line).await?;
    if line.len() > max && !line.ends_with('\n') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("request line longer than {max} bytes"),
        ));
    }
    Ok(n)
}

fn peer_uid_ok(s: &UnixStream) -> bool {
    match s.peer_cred() {
        // SAFETY: getuid has no preconditions.
        Ok(c) => c.uid() == unsafe { libc::getuid() },
        Err(_) => false,
    }
}

/// Bind the control socket; refuses if another server is serving this session.
pub fn bind(path: &Path) -> Result<UnixListener> {
    if path.exists() {
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            anyhow::bail!("a vibeke server is already running on {}", path.display());
        }
        let _ = std::fs::remove_file(path);
    }
    let l = std::os::unix::net::UnixListener::bind(path)
        .with_context(|| format!("bind {}", path.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    l.set_nonblocking(true)?;
    Ok(UnixListener::from_std(l)?)
}

/// Run the server until stopped.
pub async fn serve(server: Arc<Server>, listener: UnixListener) -> Result<()> {
    std::fs::write(server.paths.pidfile(), std::process::id().to_string())?;
    let recovered = server.recover()?;
    tracing::info!(recovered, socket = %server.paths.socket().display(), "server ready");
    let hk = server.clone();
    tokio::spawn(async move {
        // Event-driven (spec 10 §1.3): archived rows or a storage failure wake it, and writes
        // are still batched at most once a second; an idle server only wakes for the hourly
        // prune.
        let hour = Duration::from_secs(3600);
        let mut prune_at = tokio::time::Instant::now() + hour;
        loop {
            tokio::select! {
                _ = hk.housekeeping_wake.notified() => {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    hk.housekeeping();
                    if hk.degraded.lock().unwrap().is_some() {
                        // Keep probing storage once a second until it recovers.
                        hk.housekeeping_wake.notify_one();
                    }
                }
                _ = tokio::time::sleep_until(prune_at) => {
                    hk.housekeeping();
                    crate::hardening::sweep(&hk);
                    hk.archive_retention();
                    crate::assist::sweep(&hk);
                    prune_at += hour;
                }
            }
        }
    });
    crate::agents::start(&server);
    crate::task_workspace::start(&server);
    crate::preview::start(&server);
    crate::screenshots::start(&server);
    crate::desk::start(&server);
    crate::sandbox::restore(&server).await;
    crate::compat::start(&server);
    crate::plugin_native::start(&server);
    crate::inbox::start(&server);
    crate::config_api::start(&server);
    crate::security::start(&server);
    crate::privacy::start(&server);
    crate::orch::start(&server);
    crate::machines::start(&server);
    crate::handoff::start(&server);
    // Uploads made before the blob stores were unified are ingested off the async threads.
    let adopt = server.clone();
    tokio::task::spawn_blocking(move || {
        crate::blob_store::adopt_legacy(&adopt);
    });
    crate::items::start(&server);
    crate::collision::start(&server);
    crate::assist::start(&server);
    crate::gateway_supervisor::start(&server);
    let sd = server.clone();
    tokio::spawn(async move {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("sigterm");
        let mut int = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .expect("sigint");
        tokio::select! { _ = term.recv() => {}, _ = int.recv() => {} }
        // Graceful stop: holders keep running; snapshot first so the next server replays little.
        for rt in sd.panes.lock().unwrap().values() {
            rt.send(crate::pane::PaneCmd::Snapshot);
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        sd.housekeeping();
        crate::gateway_supervisor::shutdown(&sd).await;
        crate::machines::stopped(&sd, "signal");
        let _ = sd.ui.send(crate::UiEvent::Goodbye("server stopped".into()));
        tokio::time::sleep(Duration::from_millis(100)).await;
        std::process::exit(0);
    });
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(e) => {
                // EMFILE/ENFILE and the like are transient: keep serving, retry shortly.
                tracing::warn!(error = %e, "control socket accept failed; retrying");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        if !peer_uid_ok(&stream) {
            continue;
        }
        let peer_pid = stream.peer_cred().ok().and_then(|c| c.pid());
        let srv = server.clone();
        tokio::spawn(async move {
            if let Err(e) = connection(srv, stream, peer_pid).await {
                tracing::debug!(error = %e, "connection ended");
            }
        });
    }
}

/// One control connection: newline-delimited JSON-RPC, pipelined, with optional switch to the
/// binary render stream (`render.attach`) or event subscriptions.
/// The pane whose process tree contains `pid`, if any (09 §3.2): a process running inside a
/// pane gets pane scope whether or not it presents its token.
pub fn ancestry_pane(server: &Server, pid: Option<i32>) -> Option<String> {
    let pid = pid? as u32;
    let mut roots: Vec<(u32, String)> = server.with_core(|c| {
        c.model
            .panes
            .iter()
            .filter_map(|p| p.child_pid.map(|cp| (cp, p.id.clone())))
            .collect()
    });
    // Linux: the holder is the child subreaper of its pane (vk-hold), so a daemonized
    // descendant of the pane reparents to the holder rather than to init. Reaching a pane's
    // holder therefore also counts as that pane.
    if cfg!(target_os = "linux") {
        let holders = server.with_core(|c| c.store.holders()).unwrap_or_default();
        roots.extend(
            holders
                .into_iter()
                .filter_map(|h| h.holder_pid.filter(|p| *p > 1).map(|hp| (hp, h.pane))),
        );
    }
    ancestry_match(&roots, pid, std::process::id(), |p| {
        vk_hold::procinfo::info(p).map(|i| i.ppid)
    })
}

/// The pane of `roots` (`(pid, pane)`) that `pid` or one of its ancestors is, walking parents
/// with `ppid_of`. The walk stops at init and at `own` (this server: a server started from a
/// pane is not inside it for its own helpers).
fn ancestry_match(
    roots: &[(u32, String)],
    mut pid: u32,
    own: u32,
    ppid_of: impl Fn(u32) -> Option<u32>,
) -> Option<String> {
    if roots.is_empty() {
        return None;
    }
    for _ in 0..64 {
        if let Some((_, pane)) = roots.iter().find(|(cp, _)| *cp == pid) {
            return Some(pane.clone());
        }
        if pid == own {
            return None;
        }
        let ppid = ppid_of(pid)?;
        if ppid <= 1 || ppid == pid {
            return None;
        }
        pid = ppid;
    }
    None
}

/// Whether `pid` runs inside a pane of *any* session under the Vibeke runtime root
/// `runtime_root` (every session of this installation on the machine): one of its ancestors is
/// a pane holder (`vibeke hold --spec <runtime_root>/<session>/spawn-…`). Used where a pane of
/// another session must not pass for an operator (the Herdr shim's session switch, 07 §8.2).
pub fn inside_any_pane(pid: Option<i32>, runtime_root: &std::path::Path) -> bool {
    let Some(mut pid) = pid.filter(|p| *p > 1).map(|p| p as u32) else {
        return false;
    };
    let roots = [
        Some(runtime_root.to_path_buf()),
        runtime_root.canonicalize().ok(),
    ];
    let ours = |spec: &str| {
        roots
            .iter()
            .flatten()
            .any(|r| std::path::Path::new(spec).starts_with(r))
    };
    for _ in 0..64 {
        // A helper this server spawned outside any pane (reached before any holder): ours,
        // even when this server itself was started from inside a pane.
        if pid == std::process::id() {
            return false;
        }
        let Some(info) = vk_hold::procinfo::info(pid) else {
            return false;
        };
        let spec = info
            .argv
            .iter()
            .position(|a| a == "--spec")
            .and_then(|i| info.argv.get(i + 1));
        if info.argv.get(1).is_some_and(|a| a == "hold") && spec.is_some_and(|s| ours(s)) {
            return true;
        }
        if info.ppid <= 1 || info.ppid == pid {
            return false;
        }
        pid = info.ppid;
    }
    false
}

/// The pane of *this* session whose token the peer process carries in its environment
/// (`VIBEKE_PANE_TOKEN`), for a pane process the ancestry walk missed. Best effort: the
/// environment is not readable on every platform.
fn env_pane(server: &Server, pid: Option<i32>) -> Option<String> {
    let pid = pid.filter(|p| *p > 1)? as u32;
    let env = vk_hold::procinfo::environ(pid);
    let tok = env
        .iter()
        .find(|(k, _)| k == "VIBEKE_PANE_TOKEN")
        .map(|(_, v)| v.as_str())
        .filter(|t| !t.is_empty())?;
    server.pane_for_token(tok)
}

/// Whether the peer runs inside a pane of *another* session of this installation (09 §3.2):
/// one of its ancestors is a pane holder under the shared runtime root, or its environment
/// carries a pane identity (`VIBEKE_PANE_TOKEN` / `VIBEKE_PANE_ULID`) that is not one of ours
/// and whose socket, if named, lives under the same runtime root. Such a process is nobody's
/// operator: without a valid token of this session it is refused (never full scope).
pub fn foreign_pane(server: &Server, pid: Option<i32>) -> bool {
    let root = server
        .paths
        .runtime
        .parent()
        .unwrap_or(&server.paths.runtime);
    if inside_any_pane(pid, root) {
        return true;
    }
    let Some(pid) = pid.filter(|p| *p > 1).map(|p| p as u32) else {
        return false;
    };
    let env = vk_hold::procinfo::environ(pid);
    let get = |k: &str| {
        env.iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.is_empty())
    };
    let token = get("VIBEKE_PANE_TOKEN");
    if token.is_none() && get("VIBEKE_PANE_ULID").is_none() {
        return false;
    }
    if token.is_some_and(|t| server.pane_for_token(t).is_some()) {
        return false;
    }
    match get("VIBEKE_SOCKET") {
        None => true,
        Some(s) => {
            let s = std::path::Path::new(s);
            let roots = [Some(root.to_path_buf()), root.canonicalize().ok()];
            roots.iter().flatten().any(|r| s.starts_with(r))
        }
    }
}

/// A pane's identity variables. A server spawned from inside a pane (the CLI's auto-start,
/// `session.create`) must not inherit them: they would make the new server and its helpers look
/// like that pane to everyone (09 §3.2).
pub const PANE_IDENTITY_ENV: &[&str] = &[
    "VIBEKE_PANE_TOKEN",
    "VIBEKE_ELEVATED_TOKEN",
    "VIBEKE_PANE_ID",
    "VIBEKE_PANE_ULID",
    "VIBEKE_WORKSPACE_ID",
    "VIBEKE_TAB_ID",
];

/// The client id a `client.hello {client_id}` binds the connection to. A pane-scoped caller's
/// id is namespaced by its pane: it cannot take a user client's id, whose disconnect would
/// then withdraw that client's pending approvals or end its screencasts.
fn bound_client_id(pane_scope: Option<&str>, asked: &str) -> String {
    match pane_scope {
        Some(p) => format!("pane:{p}:{asked}"),
        None => asked.to_string(),
    }
}

/// Whether the connection is remote, decided by the server: a gateway (which relays phones
/// and browsers) always is. `client.hello {remote: true}` may mark any connection remote
/// (it only narrows what the caller sees), never clear it.
fn hello_remote(ctx: &Ctx, p: &Value) -> bool {
    ctx.remote || ctx.kind == "gateway" || p.get("remote").and_then(Value::as_bool) == Some(true)
}

/// Refusal for a connection from a pane of another session that presented no token of ours.
fn foreign_refusal(method: &str) -> vk_proto::rpc::RpcError {
    err(
        ErrorKind::PermissionDenied,
        format!(
            "foreign_pane: {method} refused: this process runs inside a pane of another session; present this session's pane token (client.hello {{token}}) or run the command outside any pane"
        ),
    )
    .details(json!({"scope": "foreign_pane"}))
}

/// Connection-level checks that apply to every method, before dispatch: a pane of another
/// session without our token gets nothing, and a read-only connection (`client.hello
/// {readonly: true}`) no mutating method (the catalog's `mutating` flag; unknown methods count
/// as mutating).
fn connection_gate(
    foreign: bool,
    readonly: bool,
    method: &str,
) -> Result<(), vk_proto::rpc::RpcError> {
    if foreign && method != "client.hello" {
        return Err(foreign_refusal(method));
    }
    if readonly && method != "client.hello" && crate::session_api::is_mutating(method) {
        return Err(crate::session_api::readonly_refusal(method));
    }
    Ok(())
}

/// Per-connection cleanup that must run however the connection ends (EOF, error, panic
/// unwinding): client-held agent-browser screencast subscriptions and take-overs, and a
/// gateway's connected-devices report.
struct ConnGuard {
    server: Arc<Server>,
    client_ids: Arc<std::sync::Mutex<Vec<String>>>,
    /// `events.subscribe` tasks of this connection, by subscription id (`events.unsubscribe`).
    subs: Subs,
    /// Cleared when the connection ends; dispatched calls run in its scope
    /// (`approve::CONN_OPEN`), so an approval ask still being prepared isn't registered.
    open: Arc<std::sync::atomic::AtomicBool>,
}

type Subs = Arc<std::sync::Mutex<std::collections::HashMap<String, tokio::task::AbortHandle>>>;

impl Drop for ConnGuard {
    fn drop(&mut self) {
        // Before `approve::client_gone`: an ask registering after it sees the closed flag.
        self.open.store(false, std::sync::atomic::Ordering::SeqCst);
        // The connection's event subscriptions end with it.
        for (_, h) in self.subs.lock().unwrap().drain() {
            h.abort();
        }
        let ids = std::mem::take(&mut *self.client_ids.lock().unwrap());
        for id in ids {
            crate::agent_browser::client_gone(&self.server, &id);
            // A pane's CLI waiting on an approval request withdraws it (Ctrl-C).
            crate::approve::client_gone(&self.server, &id);
            // A gateway's connected-devices report (the TUI's 📱 count) ends with it.
            crate::gateway_api::client_gone(&self.server, &id);
        }
    }
}

pub async fn connection<S>(server: Arc<Server>, stream: S, peer_pid: Option<i32>) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let guard = ConnGuard {
        server: server.clone(),
        client_ids: Arc::default(),
        subs: Arc::default(),
        open: Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    let (rd, mut wr) = tokio::io::split(stream);
    let mut rd = BufReader::new(rd);
    // A native plugin's process tree (09 §6): refused until it presents its plugin token —
    // never anonymous full scope, never a pane or elevated identity.
    let plugin_peer = crate::plugin_native::peer_plugin(&server, peer_pid);
    let ancestry = if plugin_peer.is_some() {
        None
    } else {
        ancestry_pane(&server, peer_pid).or_else(|| env_pane(&server, peer_pid))
    };
    // A pane of another session (09 §3.2): refused until it presents a token of ours.
    let mut foreign =
        plugin_peer.is_none() && ancestry.is_none() && foreign_pane(&server, peer_pid);
    let mut ctx = Ctx {
        client_id: format!("c-{}", &ulid()[20..]),
        kind: if ancestry.is_some() {
            "agent".into()
        } else {
            "anonymous".into()
        },
        pane_scope: ancestry.clone(),
        remote: false,
    };
    guard.client_ids.lock().unwrap().push(ctx.client_id.clone());
    // `client.hello {readonly: true}` (`vibeke attach --readonly`): sticky for the connection.
    let mut readonly = false;
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    let mut line = String::new();
    // Revoking a plugin token closes the connections holding it.
    let mut revoked = crate::auth::revocations(&server);
    // Handle lines until render.attach (which needs the raw stream) or EOF.
    let attach = loop {
        tokio::select! {
            Ok(()) = revoked.changed() => {
                if crate::plugin_native::is_plugin_kind(&ctx.kind)
                    && crate::plugin_native::authorize_live(&server, &ctx).is_err()
                {
                    break None;
                }
            }
            n = read_line_capped(&mut rd, &mut line, MAX_CONTROL_LINE) => {
                if n? == 0 { break None }
                let l = std::mem::take(&mut line);
                let l = l.trim_end_matches(['\n', '\r']);
                if l.is_empty() { continue }
                let Ok(req) = serde_json::from_str::<Request>(l) else {
                    let _ = out_tx.send(api::handle_line(&server, &ctx, l).await);
                    continue;
                };
                let unauth_plugin = plugin_peer
                    .as_deref()
                    .filter(|_| !crate::plugin_native::is_plugin_kind(&ctx.kind));
                if req.method != "render.attach"
                    && let Err(e) = match unauth_plugin {
                        Some(p) if req.method != "client.hello" => {
                            Err(crate::plugin_native::tokenless_refusal(p, &req.method))
                        }
                        _ => connection_gate(foreign, readonly, &req.method),
                    }
                {
                    let r = Response::err(req.id.clone().unwrap_or(Value::Null), e);
                    let _ = out_tx.send(serde_json::to_string(&r)?);
                    continue;
                }
                match req.method.as_str() {
                    "client.hello" => {
                        if let Some(tok) = req.params.get("token").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                            // A native plugin's capability-scoped token (07 §7.3, 09 §3.2).
                            if let Some(k) = crate::plugin_native::hello(&server, tok)
                                .filter(|k| plugin_peer.as_deref().is_none_or(|p| crate::plugin_native::kind_matches_peer(k, p)))
                            {
                                ctx.kind = k; ctx.pane_scope = None; foreign = false;
                                let _ = out_tx.send(api::handle_line(&server, &ctx, l).await);
                                continue;
                            }
                            // A plugin's process tree holds no other identity than its own token.
                            if let Some(p) = &plugin_peer {
                                let r = Response::err(req.id.clone().unwrap_or(Value::Null), crate::plugin_native::tokenless_refusal(p, "client.hello"));
                                let _ = out_tx.send(serde_json::to_string(&r)?);
                                continue;
                            }
                            // An approved elevation (09 §3.2): full scope, bound to the pane it was
                            // issued to. Never for a pane of another session.
                            let elevated = (!foreign)
                                .then(|| crate::auth::elevated_hello(&server, tok, ancestry.as_deref()))
                                .flatten();
                            if let Some(k) = &elevated { ctx.pane_scope = None; ctx.kind = k.clone(); }
                            match server.pane_for_token(tok) {
                                _ if elevated.is_some() => {}
                                // A token can't widen or switch scope away from the caller's own pane.
                                Some(p) if ancestry.as_ref().is_none_or(|a| *a == p) => {
                                    ctx.pane_scope = Some(p);
                                    // A valid pane token of ours: that pane's scope, not foreign.
                                    foreign = false;
                                }
                                _ => {
                                    let r = Response::err(req.id.clone().unwrap_or(Value::Null), err(ErrorKind::PermissionDenied, crate::auth::unknown_token_message(&server, ancestry.as_deref())));
                                    let _ = out_tx.send(serde_json::to_string(&r)?);
                                    continue;
                                }
                            }
                        }
                        if let Some(k) = req.params.get("kind").and_then(Value::as_str).filter(|_| !ctx.kind.starts_with(crate::auth::ELEVATED_KIND) && !crate::plugin_native::is_plugin_kind(&ctx.kind)) { ctx.kind = k.into(); }
                        if let Some(c) = req.params.get("client_id").and_then(Value::as_str) {
                            let c = bound_client_id(ctx.pane_scope.as_deref(), c);
                            ctx.client_id = c.clone();
                            guard.client_ids.lock().unwrap().push(c);
                        }
                        ctx.remote = hello_remote(&ctx, &req.params);
                        readonly |= req.params.get("readonly").and_then(Value::as_bool) == Some(true);
                        crate::notify::record_host(&server, &ctx.client_id, req.params.get("host"));
                        let _ = out_tx.send(api::handle_line(&server, &ctx, l).await);
                    }
                    "render.attach" => break Some(req),
                    "events.subscribe" => {
                        // The same per-call checks as every dispatched method (pane scope,
                        // revocation, elevation expiry); the subscription re-checks them while
                        // it runs.
                        let auth = api::authorize(&server, &ctx, &req.method, &req.params)
                            .and_then(|()| crate::auth::authorize(&server, &ctx, &req.method));
                        if let Err(e) = auth {
                            let r = Response::err(req.id.clone().unwrap_or(Value::Null), e);
                            let _ = out_tx.send(serde_json::to_string(&r)?);
                            continue;
                        }
                        if let Some((sid, h)) = subscribe(&server, &ctx, &req, out_tx.clone(), ctx.pane_scope.is_some())? {
                            let mut subs = guard.subs.lock().unwrap();
                            subs.retain(|_, h| !h.is_finished());
                            subs.insert(sid, h);
                        }
                    }
                    "events.unsubscribe" => {
                        let id = req.id.clone().unwrap_or(Value::Null);
                        let r = match req.params.get("subscription_id").and_then(Value::as_str) {
                            Some(sid) => {
                                // Idempotent: unknown, finished or already removed ids answer false.
                                let h = guard.subs.lock().unwrap().remove(sid);
                                let live = h.is_some_and(|h| {
                                    let live = !h.is_finished();
                                    h.abort();
                                    live
                                });
                                Response::ok(id, json!({"unsubscribed": live}))
                            }
                            None => Response::err(id, invalid("missing param `subscription_id`")),
                        };
                        let _ = out_tx.send(serde_json::to_string(&r)?);
                    }
                    // The Herdr shim reaches another session through this method. A process
                    // inside a pane of another session is not this session's operator, with or
                    // without its pane token (07 §8.2, 09 §6).
                    "compat.herdr.call" if ctx.pane_scope.is_none()
                        && inside_any_pane(peer_pid, server.paths.runtime.parent().unwrap_or(&server.paths.runtime)) => {
                        let r = Response::err(req.id.clone().unwrap_or(Value::Null), err(ErrorKind::PermissionDenied, "a pane cannot select another session"));
                        let _ = out_tx.send(serde_json::to_string(&r)?);
                    }
                    _ => {
                        let (srv, c, tx, l) = (server.clone(), ctx.clone(), out_tx.clone(), l.to_string());
                        let open = guard.open.clone();
                        tokio::spawn(crate::approve::CONN_OPEN.scope(open, async move { let _ = tx.send(api::handle_line(&srv, &c, &l).await); }));
                    }
                }
            }
            Some(out) = out_rx.recv() => {
                wr.write_all(out.as_bytes()).await?;
                wr.write_all(b"\n").await?;
                while let Ok(more) = out_rx.try_recv() {
                    wr.write_all(more.as_bytes()).await?;
                    wr.write_all(b"\n").await?;
                }
                wr.flush().await?;
            }
        }
    };
    // Flush pending responses.
    while let Ok(more) = out_rx.try_recv() {
        wr.write_all(more.as_bytes()).await?;
        wr.write_all(b"\n").await?;
    }
    if let Some(req) = attach {
        // A render session runs as the connection's authenticated caller: never a pane, never
        // a pane of another session, and an elevated caller stays elevated (expiry and
        // revocation end the session; `auth.elevate.decide` stays refused).
        let refusal = if ctx.pane_scope.is_some() {
            Some(err(
                ErrorKind::PermissionDenied,
                "render.attach needs a user client",
            ))
        } else if foreign {
            Some(foreign_refusal("render.attach"))
        } else if let Some(p) = plugin_peer
            .as_deref()
            .filter(|_| !crate::plugin_native::is_plugin_kind(&ctx.kind))
        {
            Some(crate::plugin_native::tokenless_refusal(p, "render.attach"))
        } else {
            // A plugin token needs the explicit `render_attach` capability (09 §6); the
            // session keeps the plugin identity and ends with its token.
            crate::auth::authorize(&server, &ctx, "render.attach")
                .and_then(|()| {
                    crate::plugin_native::authorize(&server, &ctx, "render.attach", &req.params)
                })
                .err()
        };
        if let Some(e) = refusal {
            let r = Response::err(req.id.unwrap_or(Value::Null), e);
            wr.write_all(serde_json::to_string(&r)?.as_bytes()).await?;
            wr.write_all(b"\n").await?;
            return Ok(());
        }
        let client_id = req
            .params
            .get("client_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or(ctx.client_id.clone());
        guard.client_ids.lock().unwrap().push(client_id.clone());
        let remote = req
            .params
            .get("remote")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        // 07 §3: the render stream is positional postcard; a client of another protocol
        // version would mis-decode every frame. Refuse with an upgrade hint.
        let client_protocol = req
            .params
            .get("protocol")
            .and_then(Value::as_u64)
            .unwrap_or(1);
        if client_protocol != vk_proto::render::PROTOCOL as u64 {
            let mut e = err(
                ErrorKind::Unsupported,
                format!(
                    "{}: this server speaks render protocol {}, the client {client_protocol}; upgrade the older side (`vibeke machine upgrade <machine>` for a remote, or restart the client from the same vibeke build)",
                    vk_proto::render::VERSION_MISMATCH,
                    vk_proto::render::PROTOCOL
                ),
            )
            .details(json!({
                "server_protocol": vk_proto::render::PROTOCOL,
                "client_protocol": client_protocol,
                "server_version": vk_proto::VERSION,
            }));
            e.data.kind = vk_proto::render::VERSION_MISMATCH.into();
            let r = Response::err(req.id.clone().unwrap_or(Value::Null), e);
            wr.write_all(serde_json::to_string(&r)?.as_bytes()).await?;
            wr.write_all(b"\n").await?;
            wr.flush().await?;
            return Ok(());
        }
        let max_fps = req
            .params
            .get("caps")
            .and_then(|c| c.get("max_fps"))
            .and_then(Value::as_u64)
            .unwrap_or(120) as u32;
        let r = Response::ok(
            req.id.unwrap_or(Value::Null),
            json!({"protocol": vk_proto::render::PROTOCOL, "client_id": client_id,
                   "features": render::FEATURES}),
        );
        wr.write_all(serde_json::to_string(&r)?.as_bytes()).await?;
        wr.write_all(b"\n").await?;
        wr.flush().await?;
        // Host terminal for click-to-focus raising and native notifications (08 §7.1).
        crate::notify::record_host(&server, &client_id, req.params.get("host"));
        if readonly {
            crate::session_api::set_readonly(&client_id, true);
        }
        let auth = render::Auth {
            kind: ctx.kind.clone(),
            readonly,
        };
        let r = render::serve_as(
            server.clone(),
            rd,
            wr,
            client_id.clone(),
            remote,
            max_fps,
            auth,
        )
        .await;
        if readonly {
            crate::session_api::set_readonly(&client_id, false);
        }
        crate::theme::forget_client(&server, &client_id);
        r?;
        return Ok(());
    }
    wr.flush().await?;
    Ok(())
}

/// `events.subscribe {after?, types?}`: backlog from the outbox, then live events; never silent
/// loss (overflow closes the subscription with `events.overflow`). A revoked or expired caller's
/// subscription ends with `events.closed {subscription_id, reason}`. Returns the subscription id
/// and its task (aborted by `events.unsubscribe` or when the connection ends).
fn subscribe(
    server: &Arc<Server>,
    ctx: &Ctx,
    req: &Request,
    out: mpsc::UnboundedSender<String>,
    pane_scoped: bool,
) -> Result<Option<(String, tokio::task::AbortHandle)>> {
    let id = req.id.clone().unwrap_or(Value::Null);
    let p = req.params.clone();
    let after = match api::after_seq(server, &p) {
        Ok(a) => a,
        Err(e) => {
            let _ = out.send(serde_json::to_string(&Response::err(id, e))?);
            return Ok(None);
        }
    };
    let types: Vec<String> = match p.get("types") {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        Some(Value::String(s)) => s.split(',').map(str::to_string).collect(),
        _ => vec![],
    };
    let sub_id = format!("s{}", &ulid()[20..]);
    let mut rx = server.events.subscribe();
    let at = api::cursor(server, None);
    let replay = after > 0 || p.get("after").is_some();
    // Live-only: the position is fixed now (with the receiver already subscribed), not when the
    // task first runs — an event committed in between would otherwise be skipped.
    let live_from = (!replay).then(|| server.with_core(|c| c.store.last_seq().unwrap_or(0)));
    // A gateway counts as connected (`gateway.status`) from before the subscription is
    // acknowledged until its stream ends.
    let presence = crate::gateway_bridge::enter(server, ctx);
    let _ = out.send(serde_json::to_string(&Response::ok(
        id,
        json!({"subscription_id": sub_id, "at": at}),
    ))?);
    let srv = server.clone();
    let ret_id = sub_id.clone();
    let ctx = ctx.clone();
    let mut revoked = crate::auth::revocations(server);
    let task = tokio::spawn(async move {
        // The guard fails the requests waiting on this gateway when the task ends or is aborted.
        let _gateway = presence;
        let notify = |e: &vk_store::Event| {
            serde_json::to_string(&json!({"jsonrpc": "2.0", "method": "events.event", "params": {"subscription_id": sub_id, "event": e}})).unwrap()
        };
        // Revocation or elevation expiry ends the subscription (09 §3.2): checked before every
        // delivery and whenever a revocation happens or the elevation's lifetime runs out.
        let closed = |e: vk_proto::rpc::RpcError| {
            serde_json::to_string(&json!({"jsonrpc": "2.0", "method": "events.closed", "params": {"subscription_id": sub_id, "reason": e.message}})).unwrap()
        };
        let allowed = || crate::auth::authorize(&srv, &ctx, "events.subscribe");
        let mut last = after;
        if replay {
            loop {
                let batch = srv
                    .with_core(|c| c.store.events_after(last, 500, &types))
                    .unwrap_or_default();
                if batch.is_empty() {
                    break;
                }
                for e in &batch {
                    if let Err(err) = allowed() {
                        let _ = out.send(closed(err));
                        return;
                    }
                    last = e.seq;
                    if out.send(notify(e)).is_err() {
                        return;
                    }
                }
            }
        } else {
            last = live_from.unwrap_or(0);
        }
        loop {
            let expiry = crate::auth::elevation_expiry(&srv, &ctx.kind).map(|ms| {
                let left = (ms - vk_store::now_ms()).max(0) as u64;
                tokio::time::Instant::now() + Duration::from_millis(left + 1)
            });
            let ev = tokio::select! {
                ev = rx.recv() => ev,
                _ = revoked.changed() => {
                    if let Err(err) = allowed() {
                        let _ = out.send(closed(err));
                        return;
                    }
                    continue;
                }
                _ = async { tokio::time::sleep_until(expiry.unwrap()).await }, if expiry.is_some() => {
                    if let Err(err) = allowed() {
                        let _ = out.send(closed(err));
                        return;
                    }
                    continue;
                }
            };
            match ev {
                Ok(e) => {
                    if e.seq == 0 {
                        // A transient notification (`assistant.delta`): not outbox history, so
                        // it never moves the cursor, and generated text goes to full-scope
                        // subscribers only (14 §9).
                        if pane_scoped && e.kind.starts_with("assistant.") {
                            continue;
                        }
                        // Addressed to one gateway: its params may hold an invitation link.
                        if e.kind == "gateway.request"
                            && !crate::gateway_bridge::deliver_to(&e, &ctx)
                        {
                            continue;
                        }
                    } else {
                        if e.seq <= last {
                            continue;
                        }
                        last = e.seq;
                    }
                    if !types.is_empty() && !types.iter().any(|g| vk_store::glob_match(g, &e.kind))
                    {
                        continue;
                    }
                    if let Err(err) = allowed() {
                        let _ = out.send(closed(err));
                        return;
                    }
                    if out.send(notify(&e)).is_err() {
                        return;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    let c = api::cursor(&srv, Some(last));
                    let _ = out.send(serde_json::to_string(&json!({"jsonrpc":"2.0","method":"events.overflow","params":{"subscription_id": sub_id, "resume_from": c}})).unwrap());
                    return;
                }
                Err(_) => return,
            }
        }
    });
    Ok(Some((ret_id, task.abort_handle())))
}

// ---- tasks (05) -----------------------------------------------------------------------------

fn task_cfg(p: &Value) -> vk_tasks::WorktreeConfig {
    let mut cfg = vk_tasks::WorktreeConfig::default();
    if let Some(root) = s(p, "root")
        && let Ok(r) = vk_tasks::WorktreeRoot::parse(root)
    {
        cfg.root = r;
    }
    if let Some(t) = s(p, "branch_template") {
        cfg.branch_template = t.to_string();
    }
    if let Some(f) = p.get("fetch").and_then(Value::as_bool) {
        cfg.fetch_before_create = f;
    }
    cfg
}

/// blake3 over the `.vibeke/` tree (relative paths + contents, sorted; logs excluded).
pub fn vibeke_dir_digest(root: &std::path::Path) -> Option<String> {
    let dir = root.join(".vibeke");
    if !dir.is_dir() {
        return None;
    }
    let mut files = Vec::new();
    let mut stack = vec![dir.clone()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).ok()?.flatten() {
            let path = e.path();
            let ft = e.file_type().ok()?;
            if ft.is_dir() {
                stack.push(path);
            } else if !path.extension().is_some_and(|x| x == "log") {
                files.push(path);
            }
        }
    }
    files.sort();
    let mut h = blake3::Hasher::new();
    for f in files {
        h.update(f.strip_prefix(&dir).ok()?.to_string_lossy().as_bytes());
        h.update(&[0]);
        h.update(&std::fs::read(&f).ok()?);
        h.update(&[0]);
    }
    Some(h.finalize().to_hex().to_string())
}

fn trust_map(server: &Server) -> std::collections::HashMap<String, String> {
    server.with_core(|c| {
        c.store
            .kv_get("security", "repo_trust")
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    })
}

pub(crate) fn repo_trusted(server: &Server, repo: &std::path::Path, digest: &str) -> bool {
    let key = repo
        .canonicalize()
        .unwrap_or_else(|_| repo.to_path_buf())
        .to_string_lossy()
        .into_owned();
    trust_map(server).get(&key).is_some_and(|d| d == digest)
}

/// Devcontainer lifecycle commands and image builds (13 §9) run only for the exact
/// `devcontainer.json` content the user trusted with `policy.trust` (09 §4).
pub(crate) fn devcontainer_trusted(server: &Server, repo: &std::path::Path, digest: &str) -> bool {
    let key = format!(
        "{}#devcontainer",
        repo.canonicalize()
            .unwrap_or_else(|_| repo.to_path_buf())
            .to_string_lossy()
    );
    trust_map(server).get(&key).is_some_and(|d| d == digest)
}

/// `policy.trust {path}` (full scope only): record the digest of the repo's `.vibeke/` tree and
/// return what was trusted, including the setup script's content.
pub fn policy_trust(server: &Server, p: &Value) -> R {
    let path = std::path::PathBuf::from(s(p, "path").unwrap_or("."));
    let repo = vk_tasks::repo_root(&path).map(|i| i.root).unwrap_or(path);
    let repo = repo
        .canonicalize()
        .map_err(|e| invalid(format!("{}: {e}", repo.display())))?;
    let digest = vibeke_dir_digest(&repo);
    // A devcontainer's lifecycle commands/image build are repo automation too (13 §9).
    let dc = vk_sandbox::devcontainer::load(&repo, None).ok().flatten();
    let dc_digest = dc.as_ref().map(vk_sandbox::devcontainer::digest);
    if digest.is_none() && dc_digest.is_none() {
        return Err(invalid(format!(
            "{} has no .vibeke/ directory or devcontainer",
            repo.display()
        )));
    }
    if let Some(want) = s(p, "digest").filter(|d| Some(*d) != digest.as_deref()) {
        return Err(err(
            ErrorKind::Conflict,
            format!(
                ".vibeke/ changed since review (expected {want}, now {})",
                digest.as_deref().unwrap_or("none")
            ),
        ));
    }
    if let Some(want) = s(p, "devcontainer_digest").filter(|d| Some(*d) != dc_digest.as_deref()) {
        return Err(err(
            ErrorKind::Conflict,
            format!("devcontainer changed since review (expected {want})"),
        ));
    }
    let mut map = trust_map(server);
    if let Some(d) = &digest {
        map.insert(repo.to_string_lossy().into_owned(), d.clone());
    }
    if let Some(d) = &dc_digest {
        map.insert(
            format!("{}#devcontainer", repo.to_string_lossy()),
            d.clone(),
        );
    }
    let script = std::fs::read_to_string(repo.join(".vibeke/setup.sh")).ok();
    let devcontainer = dc.as_ref().map(|d| {
        json!({
            "file": d.path, "digest": dc_digest, "image": d.image,
            "build": d.build.as_ref().map(|b| json!({"dockerfile": b.dockerfile, "context": b.context})),
            "lifecycle": d.lifecycle.iter().map(|(n, c)| json!({"name": n, "command": c.script()})).collect::<Vec<_>>(),
            "warnings": d.warnings,
        })
    });
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.m.kv(
        "security",
        "repo_trust",
        Some(serde_json::to_string(&map).unwrap_or_default()),
    );
    tx.event(
        "policy.repo_trusted",
        json!({}),
        json!({"repo": repo, "digest": digest, "devcontainer_digest": dc_digest}),
    );
    server.commit(&mut c, tx).map_err(internal)?;
    drop(c);
    // Repo harness manifests (04 §5) are re-evaluated on the next detection.
    crate::agents::manifests::forget_repo_trust();
    Ok(
        json!({"repo": repo, "digest": digest, "setup_script": script, "task_file": crate::task_workspace::trust_summary(&repo), "devcontainer": devcontainer, "harness_manifests": repo_manifest_argv(&repo)}),
    )
}

/// Repo harness manifests with the argv they launch/resume (09 §4 rule 4: shown at trust time).
fn repo_manifest_argv(repo: &Path) -> Vec<Value> {
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(repo.join(".vibeke/harnesses"))
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .collect();
    files.sort();
    files
        .iter()
        .filter_map(|f| {
            let text = std::fs::read_to_string(f).ok()?;
            let t: toml::Table = text.parse().ok()?;
            let id = t.get("id")?.as_str()?;
            let argv = |table: &str| {
                t.get(table)
                    .and_then(|x| x.get("argv"))
                    .and_then(|a| a.as_array())
                    .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
                    .unwrap_or_default()
            };
            Some(json!({"id": format!("repo:{id}"), "file": f, "launch": argv("launch"), "resume": argv("resume")}))
        })
        .collect()
}

pub async fn tasks_api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    Some(match method {
        m if m.starts_with("task.")
            && let Some(r) = crate::tracking::api(server, ctx, m, p).await =>
        {
            r
        }
        m if m.starts_with("task.")
            && let Some(r) = crate::review::api(server, ctx, m, p).await =>
        {
            r
        }
        "task.create" => task_create(server, ctx, p).await,
        m if (m.starts_with("sandbox.") || m == "task.sync")
            && let Some(r) = crate::sandbox::api(server, ctx, m, p).await =>
        {
            r
        }
        "policy.trust" if p.get("check").and_then(Value::as_bool) == Some(true) => {
            crate::repo_config::check(server, p)
        }
        "policy.trust" => policy_trust(server, p),
        "task.list" => Ok(json!({"tasks": server.with_core(|c| c.model.tasks.clone())})),
        "task.get" => {
            let t = match req(p, "task") {
                Ok(t) => t,
                Err(e) => return Some(Err(e)),
            };
            match server.with_core(|c| c.task(t).cloned()) {
                Some(task) => {
                    let status = task.worktree_path.as_ref().and_then(|w| {
                        vk_tasks::branch_status(Path::new(w), task.base_ref.as_deref()).ok()
                    });
                    let pr = crate::task_workspace::pr_peek(&task);
                    Ok(
                        json!({"task": task, "branch_status": status.map(|s| json!({"branch": s.branch, "ahead": s.ahead, "behind": s.behind, "dirty_files": s.dirty_files, "upstream": s.upstream, "compared_to": s.compared_to})), "pr": pr, "collisions": crate::collision::for_task(server, &task)}),
                    )
                }
                None => Err(not_found("task", t)),
            }
        }
        "task.finish" => task_finish(server, p).await,
        "task.setup" => crate::task_workspace::task_setup(server, p).await,
        "task.pr" => crate::task_workspace::task_pr(server, p).await,
        "task.reconcile" => crate::task_workspace::task_reconcile(server, p).await,
        "worktree.list" => {
            let cwd = s(p, "cwd")
                .or(s(p, "repo"))
                .map(str::to_string)
                .unwrap_or_else(|| ".".into());
            match vk_tasks::list_worktrees(Path::new(&cwd)) {
                Ok(w) => Ok(
                    json!({"worktrees": w.iter().map(|e| json!({"path": e.path, "branch": e.branch, "head": e.head, "locked": e.locked, "prunable": e.prunable, "main": e.is_main})).collect::<Vec<_>>()}),
                ),
                Err(e) => Err(invalid(e.to_string())),
            }
        }
        "worktree.create" => worktree_create(server, ctx, p).await,
        "worktree.open" => worktree_open(server, ctx, p),
        "worktree.repo_root" => {
            let cwd = s(p, "cwd").unwrap_or(".");
            match vk_tasks::repo_root(Path::new(cwd)) {
                Some(r) => {
                    Ok(json!({"repo_root": r.root, "vcs": "git", "worktree_root": r.worktree_root}))
                }
                None => Err(not_found("repo", cwd)),
            }
        }
        "worktree.remove" => {
            let path = match req(p, "path") {
                Ok(x) => x.to_string(),
                Err(e) => return Some(Err(e)),
            };
            let force = p.get("force").and_then(Value::as_bool).unwrap_or(false);
            let srv = server.clone();
            let job_id = format!("j{}", &ulid()[20..]);
            let jid = job_id.clone();
            std::thread::spawn(move || {
                let job = vk_tasks::start_remove(
                    Path::new(&path),
                    vk_tasks::RemoveOptions {
                        force,
                        protect_dirty: true,
                        ..Default::default()
                    },
                );
                let state = job.wait();
                let mut c = srv.core.lock().unwrap();
                let mut tx = Tx::new();
                tx.event(
                    "worktree.removed",
                    json!({"path": path}),
                    json!({"job": jid, "state": format!("{state:?}")}),
                );
                let _ = srv.commit(&mut c, tx);
            });
            Ok(json!({"job": job_id}))
        }
        _ => return None,
    })
}

/// `[tasks]` worktree settings from the config, overridden by request params.
fn worktree_cfg(p: &Value) -> vk_tasks::WorktreeConfig {
    let mut cfg = vk_tasks::WorktreeConfig::default();
    if let Ok((c, _)) = vk_config::Config::load(vk_config::config_path()) {
        if let Ok(r) = vk_tasks::WorktreeRoot::parse(&c.tasks.root) {
            cfg.root = r;
        }
        cfg.branch_template = c.tasks.branch_template.clone();
        cfg.fetch_before_create = c.tasks.fetch_before_create;
    }
    let o = task_cfg(p);
    if s(p, "root").is_some() {
        cfg.root = o.root;
    }
    if s(p, "branch_template").is_some() {
        cfg.branch_template = o.branch_template;
    }
    if p.get("fetch").is_some() {
        cfg.fetch_before_create = o.fetch_before_create;
    }
    cfg
}

/// A workspace already rooted at `path`, if any.
fn workspace_at(server: &Server, path: &Path) -> Option<vk_proto::model::Workspace> {
    let want = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    server.with_core(|c| {
        c.model
            .workspaces
            .iter()
            .find(|w| {
                let r = Path::new(&w.root_path);
                r.canonicalize().unwrap_or_else(|_| r.to_path_buf()) == want
            })
            .cloned()
    })
}

/// `worktree.create {repo|cwd, branch, base?, open?: false, focus?: false, name?}` →
/// `{worktree, workspace?, tab?, root_pane?}` (07 §2.10). Emits `worktree.created`.
fn no_focus_from_pane(ctx: &Ctx, p: &Value) -> Result<(), vk_proto::rpc::RpcError> {
    if ctx.pane_scope.is_some() && p.get("focus").and_then(Value::as_bool) == Some(true) {
        return Err(err(
            ErrorKind::PermissionDenied,
            "agents can't move the user's focus",
        ));
    }
    Ok(())
}

async fn worktree_create(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    no_focus_from_pane(ctx, p)?;
    let branch = req(p, "branch")?.to_string();
    let repo = s(p, "repo")
        .or(s(p, "cwd"))
        .map(str::to_string)
        .or_else(|| {
            api::resolve_pane(server, ctx, None)
                .ok()
                .and_then(|x| server.pane_cwd(&x.id))
        })
        .ok_or_else(|| invalid("repo or cwd is required"))?;
    let cfg = worktree_cfg(p);
    let slug = vk_tasks::slugify(&branch, cfg.slug_max_len);
    let creq = vk_tasks::CreateRequest {
        repo: repo.clone().into(),
        title: branch.clone(),
        base: s(p, "base").map(str::to_string),
        branch: Some(branch.clone()),
        slug: Some(slug),
    };
    let checkout = tokio::task::spawn_blocking(move || vk_tasks::create_worktree(&creq, &cfg))
        .await
        .map_err(internal)?
        .map_err(|e| match e {
            vk_tasks::Error::NotARepo(_) => not_found("repo", &repo),
            e => err(ErrorKind::Conflict, e.to_string()),
        })?;
    let path = checkout.path.to_string_lossy().into_owned();
    let wt = json!({
        "path": path,
        "branch": checkout.branch,
        "base_ref": checkout.base_ref,
        "repo_root": checkout.repo_root,
        "created_branch": checkout.created_branch,
    });
    let opened = if p.get("open").and_then(Value::as_bool).unwrap_or(false) {
        let focus = p
            .get("focus")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            .then_some(ctx.client_id.as_str());
        Some(
            server
                .create_workspace(
                    &path,
                    s(p, "name").map(str::to_string).or(checkout.branch.clone()),
                    None,
                    focus,
                )
                .map_err(internal)?,
        )
    } else {
        None
    };
    {
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.event(
            "worktree.created",
            json!({"workspace": opened.as_ref().map(|o| o.0.id.clone())}),
            wt.clone(),
        );
        server.commit(&mut c, tx).map_err(internal)?;
    }
    Ok(match opened {
        Some((ws, tab, pane)) => {
            json!({"worktree": wt, "workspace": ws, "tab": tab, "root_pane": pane, "warnings": checkout.warnings})
        }
        None => json!({"worktree": wt, "warnings": checkout.warnings}),
    })
}

/// `worktree.open {path, focus?: false, name?}` → `{worktree, workspace, created}`: reuse the
/// workspace rooted at the worktree, else create one. Emits `worktree.opened`.
fn worktree_open(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    no_focus_from_pane(ctx, p)?;
    let path = req(p, "path")?;
    let co = vk_tasks::open_worktree(Path::new(path), Path::new(path)).map_err(|e| match e {
        vk_tasks::Error::NotARepo(_) | vk_tasks::Error::WorktreeNotFound(_) => {
            not_found("worktree", path)
        }
        e => invalid(e.to_string()),
    })?;
    let focus = p.get("focus").and_then(Value::as_bool).unwrap_or(false);
    let cwd = co.path.to_string_lossy().into_owned();
    let (ws, created) = match workspace_at(server, &co.path) {
        Some(ws) => {
            if focus {
                let pane = server.with_core(|c| {
                    c.tabs_of(&ws.id)
                        .first()
                        .and_then(|t| t.focused_pane.clone())
                });
                if let Some(pane) = pane {
                    server.focus_pane(&ctx.client_id, &pane);
                }
            }
            (ws, false)
        }
        None => {
            let (ws, _, _) = server
                .create_workspace(
                    &cwd,
                    s(p, "name").map(str::to_string).or(co.branch.clone()),
                    None,
                    focus.then_some(ctx.client_id.as_str()),
                )
                .map_err(internal)?;
            (ws, true)
        }
    };
    let wt = json!({"path": cwd, "branch": co.branch, "repo_root": co.repo_root});
    {
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.event(
            "worktree.opened",
            json!({"workspace": ws.id}),
            json!({"path": cwd, "branch": co.branch, "repo_root": co.repo_root, "created_workspace": created}),
        );
        server.commit(&mut c, tx).map_err(internal)?;
    }
    Ok(json!({"worktree": wt, "workspace": ws, "created": created}))
}

async fn task_create(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let title = req(p, "title")?.to_string();
    let repo = s(p, "repo")
        .map(str::to_string)
        .or_else(|| {
            api::resolve_pane(server, ctx, None)
                .ok()
                .and_then(|x| server.pane_cwd(&x.id))
        })
        .unwrap_or_else(|| ".".into());
    // Code isolation backend (05 §4): worktree | jj_workspace | none.
    let info = crate::parity::resolve_checkout(&repo, p)?;
    // Execution isolation (13 §3): validate before creating anything.
    let mut iso_req = crate::sandbox::IsoRequest::from_params(p, &crate::sandbox::load_cfg())?;
    if iso_req.level == vk_proto::model::IsolationLevel::Vm && !crate::orch_vm::enabled(server) {
        return Err(err(
            ErrorKind::Unsupported,
            "the vm isolation level ships in M4; use --isolate sandbox",
        ));
    }
    crate::sandbox::extras::check_task_host_yolo(server, &iso_req, &info.root, p)?;
    let cfg = task_cfg(p);
    let creq = vk_tasks::CreateRequest {
        repo: info.root.clone(),
        title: title.clone(),
        base: s(p, "base").map(str::to_string),
        branch: s(p, "branch").map(str::to_string),
        slug: s(p, "slug").map(str::to_string),
    };
    let kind = info.kind;
    let checkout =
        tokio::task::spawn_blocking(move || crate::parity::create_checkout(kind, &creq, &cfg))
            .await
            .map_err(internal)?
            .map_err(|e| err(ErrorKind::Conflict, e.to_string()))?;
    // `.vibeke/task.toml` of the new checkout plus the user's per-repo override (05 §5).
    let tcfg = crate::repo_config::tasks_cfg(server, &info.root);
    let resolved = crate::task_workspace::resolve(&info.root, &checkout.path, &tcfg);
    let mut files = resolved.file.files.clone();
    files.copy = match p.get("copy_files").and_then(Value::as_array) {
        Some(a) => a
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        None => {
            let mut v = tcfg.copy_files.clone();
            for c in &files.copy {
                if !v.contains(c) {
                    v.push(c.clone());
                }
            }
            v
        }
    };
    let shared_cwd = info.kind == "none";
    let deps_spec = resolved.file.deps.clone();
    let (src_root, dst_root) = (info.root.clone(), checkout.path.clone());
    let (copied, deps_plan) = tokio::task::spawn_blocking(move || {
        let mut res = vk_tasks::materialize_files(&src_root, &dst_root, &files);
        // The dependency strategy only applies when the repo or the user configured `[deps]`;
        // a shared checkout already has its own.
        let plan = (!shared_cwd && (deps_spec.strategy.is_some() || deps_spec.install.is_some()))
            .then(|| vk_tasks::plan_deps(&deps_spec, &src_root, &dst_root));
        if let Some(pl) = &plan {
            res.extend(vk_tasks::run_deps_clone(pl, &src_root, &dst_root));
        }
        (res, plan)
    })
    .await
    .map_err(internal)?;
    let id = ulid();
    let handle = server.with_core(|c| c.next_task_handle());
    let mut warnings: Vec<String> = checkout.warnings.clone();
    warnings.extend(resolved.warnings.iter().cloned());
    // Port lease (machine-wide): `tasks.port_pool`/`port_block`, `task.create {ports}`, then
    // the task file's `[ports] count`.
    let pool = match s(p, "port_pool") {
        Some(r) => {
            vk_tasks::PortPool::parse(r, tcfg.port_block).map_err(|e| invalid(e.to_string()))?
        }
        None => vk_tasks::PortPool {
            start: tcfg.port_pool.start,
            end: tcfg.port_pool.end,
            block: tcfg.port_block,
        },
    };
    let count = p
        .get("ports")
        .and_then(Value::as_u64)
        .and_then(|n| u16::try_from(n).ok())
        .or(resolved.file.ports.count)
        .unwrap_or(pool.block);
    let leases = vk_tasks::PortLeases::new(crate::paths::state_root(), pool);
    let lease = match leases.lease_sized(
        &vk_tasks::LeaseRequest {
            task_id: id.clone(),
            session: server.opts.session.clone(),
            owner_pid: None,
        },
        count,
    ) {
        Ok(l) => Some(l),
        Err(e) => {
            warnings.push(format!("no ports leased: {e} (see `vibeke doctor`)"));
            None
        }
    };
    // Repo automation runs only after trust (09 §4): (canonical repo path, digest of `.vibeke/`).
    let digest = vibeke_dir_digest(&checkout.path);
    let trusted = digest
        .as_ref()
        .is_some_and(|d| repo_trusted(server, &info.root, d));
    let vars = crate::task_workspace::vars_for(
        &id,
        &checkout.slug,
        checkout.branch.as_deref(),
        lease.as_ref(),
        &info.root,
        &checkout.path,
    );
    let mut setup_plan = crate::task_workspace::plan_setup(
        &resolved,
        deps_plan.as_ref(),
        &vars,
        &checkout.path,
        s(p, "setup_script"),
        &tcfg.setup_script,
        trusted,
    );
    setup_plan.digest = digest.clone();
    let cwd = checkout.path.to_string_lossy().into_owned();
    let mut isolation = vk_proto::model::Isolation {
        yolo: iso_req.yolo,
        ..Default::default()
    };
    if iso_req.level != vk_proto::model::IsolationLevel::Host {
        iso_req.harnesses = p
            .get("agents")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|x| {
                        x.get("harness")
                            .and_then(Value::as_str)
                            .unwrap_or("claude")
                            .to_string()
                    })
                    .collect()
            })
            .unwrap_or_default();
        if let Some(l) = &lease {
            iso_req.local_ports.extend(l.start..=l.end);
        }
        let b =
            crate::sandbox::prepare_box(server, &id, Some(&id), &checkout.path, iso_req.clone())
                .await?;
        isolation = b.isolation.clone();
    }
    // The first pane spawns before the task is in the model: hand it the leased-port env.
    let pre_task = Task {
        port_range: lease.as_ref().map(|l| (l.start, l.end)),
        worktree_path: Some(cwd.clone()),
        ..Default::default()
    };
    let mut pane_env = crate::preview_fabric::task_port_env(&pre_task);
    for (k, v) in &setup_plan.env {
        pane_env.retain(|(n, _)| n != k);
        pane_env.push((k.clone(), v.clone()));
    }
    server
        .pending_task_env
        .lock()
        .unwrap()
        .insert(id.clone(), pane_env);
    let created = server.create_workspace_for(
        &cwd,
        Some(checkout.slug.clone()),
        None,
        p.get("focus")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            .then_some(ctx.client_id.as_str()),
        Some(&id),
    );
    server.pending_task_env.lock().unwrap().remove(&id);
    let (ws, _tab, pane) = created.map_err(internal)?;
    let task = Task {
        id: id.clone(),
        handle,
        title: title.clone(),
        slug: checkout.slug.clone(),
        workspace: Some(ws.id.clone()),
        repo_root: info.root.to_string_lossy().into_owned(),
        worktree_path: Some(cwd.clone()),
        branch: checkout.branch.clone(),
        base_ref: checkout.base_ref.clone(),
        port_range: lease.as_ref().map(|l| (l.start, l.end)),
        status: "active".into(),
        setup_status: None,
        created_at_ms: vk_store::now_ms(),
        owner_machine: server.opts.machine.clone(),
        isolation,
        checkout: Some(info.kind.to_string()),
        ..Default::default()
    };
    {
        let mut c = server.core.lock().unwrap();
        let mut w = ws.clone();
        w.task = Some(id.clone());
        w.branch = checkout.branch.clone();
        let mut tx = Tx::new();
        tx.ws(w);
        tx.task(task.clone());
        tx.counters = true;
        tx.event(
            "task.created",
            json!({"task": id, "workspace": ws.id}),
            json!({"title": title, "branch": checkout.branch, "path": cwd}),
        );
        // A task checkout is a worktree too (Herdr `worktree.created`, 07 §8.3).
        if info.kind == "worktree" {
            tx.event(
                "worktree.created",
                json!({"task": id, "workspace": ws.id}),
                json!({"path": cwd, "branch": checkout.branch, "base_ref": checkout.base_ref, "repo_root": info.root, "created_branch": checkout.created_branch}),
            );
        }
        // The env every later pane of the task gets (saved: it survives a server restart).
        tx.m.kv("task_env", &id, serde_json::to_string(&setup_plan.env).ok());
        // Names and hashes only, never contents.
        tx.event(
            "task.files_materialized",
            json!({"task": id}),
            json!({"files": copied.iter().map(crate::task_workspace::file_json).collect::<Vec<_>>(),
                   "deps": deps_plan}),
        );
        server.commit(&mut c, tx).map_err(internal)?;
    }
    // Task `[previews]` with ports from the lease (06 B2): declared before anything starts.
    let task_previews = crate::preview_fabric::declare_task_previews(
        server,
        ctx,
        &task,
        &pane.id,
        &checkout.path,
        lease.as_ref(),
        p,
    );
    // Setup (05 §7): a visible `setup` pane, after trust; agents wait for it unless
    // `setup.parallel_agent`.
    let want_setup = p.get("setup").and_then(Value::as_bool).unwrap_or(true)
        && iso_req.level != vk_proto::model::IsolationLevel::Container;
    let launch = if want_setup {
        crate::task_workspace::launch_setup(
            server,
            &id,
            &info.root,
            &checkout.path,
            &setup_plan,
            lease.clone(),
            &pane.id,
        )
    } else {
        crate::task_workspace::SetupLaunch {
            done: None,
            pane: None,
        }
    };
    let agents: Vec<Value> = p
        .get("agents")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let agent_opts = (iso_req.yolo, iso_req.level);
    let mut runs = Vec::new();
    let mut agents_pending = false;
    match launch.done {
        Some(done) if !setup_plan.parallel_agent && !agents.is_empty() => {
            agents_pending = true;
            let (srv, pane_id, task_id) = (server.clone(), pane.id.clone(), id.clone());
            let start_on_failure = setup_plan.start_agents_on_failure;
            tokio::spawn(async move {
                let ok = done.await.unwrap_or(false);
                if ok || start_on_failure {
                    if let Err(e) =
                        start_agents(&srv, &pane_id, &task_id, &agents, agent_opts, None).await
                    {
                        tracing::warn!(task = %task_id, error = %e.message, "agent start after setup failed");
                    }
                } else {
                    let mut c = srv.core.lock().unwrap();
                    let mut tx = Tx::new();
                    tx.event(
                        "task.agents_withheld",
                        json!({"task": task_id}),
                        json!({"reason": "setup failed", "hint": "fix it and run `vibeke task setup`, or set setup.start_agents_on_failure = true"}),
                    );
                    let _ = srv.commit(&mut c, tx);
                }
            });
        }
        running => {
            let note = (running.is_some() && !agents.is_empty())
                .then(|| {
                    launch
                        .pane
                        .as_ref()
                        .and_then(|sp| server.with_core(|c| c.pane(sp).map(|x| x.handle.clone())))
                })
                .flatten()
                .map(|h| format!("Setup is still running in pane {h}."));
            runs =
                start_agents(server, &pane.id, &id, &agents, agent_opts, note.as_deref()).await?;
        }
    }
    let files: Vec<Value> = copied
        .iter()
        .map(crate::task_workspace::file_json)
        .collect();
    let copied: Vec<String> = copied
        .iter()
        .filter(|c| matches!(c.outcome, vk_tasks::CopyOutcome::Copied))
        .map(|c| c.rel.clone())
        .collect();
    let task = server.with_core(|c| c.task(&id).cloned()).unwrap_or(task);
    Ok(
        json!({"task": task, "workspace": ws, "panes": [pane], "runs": runs, "copied": copied, "files": files, "deps": deps_plan, "setup": {"pane": launch.pane, "status": task.setup_status, "agents_pending": agents_pending, "commands": crate::task_workspace::commands_json(&setup_plan)}, "warnings": warnings, "previews": task_previews["previews"], "preview_warnings": task_previews["warnings"]}),
    )
}

/// Start the task's agents in its first pane (`--agent claude:impl`); `note` is appended to
/// each prompt (setup still running).
async fn start_agents(
    server: &Arc<Server>,
    pane: &str,
    task: &str,
    agents: &[Value],
    (yolo, level): (bool, vk_proto::model::IsolationLevel),
    note: Option<&str>,
) -> Result<Vec<Value>, vk_proto::rpc::RpcError> {
    let mut runs = Vec::new();
    for a in agents {
        let harness = a.get("harness").and_then(Value::as_str).unwrap_or("claude");
        let name = a.get("name").and_then(Value::as_str);
        let prompt = a
            .get("prompt")
            .and_then(Value::as_str)
            .map(|pr| match note {
                Some(n) => format!("{pr}\n\n({n})"),
                None => pr.to_string(),
            });
        let opts = crate::sandbox::LaunchOpts {
            yolo,
            isolate: Some(level),
            network: None,
            ..Default::default()
        };
        runs.push(
            crate::agents::start_in_pane_opts(
                server,
                pane,
                harness,
                name,
                prompt.as_deref(),
                &[],
                Some(task),
                &opts,
            )
            .await?,
        );
    }
    Ok(runs)
}

async fn task_finish(server: &Arc<Server>, p: &Value) -> R {
    let t = req(p, "task")?;
    let task = server
        .with_core(|c| c.task(t).cloned())
        .ok_or_else(|| not_found("task", t))?;
    let remove = p
        .get("remove_worktree")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let force = p.get("force").and_then(Value::as_bool).unwrap_or(false);
    if task.ownership == vk_proto::model::TaskOwnership::Attached {
        // 15 §4.3: an attached task's lifecycle never stops processes, closes the workspace,
        // releases ports or deletes files. Only the record changes.
        return crate::tracking::finish_attached(
            server,
            &task,
            s(p, "status").unwrap_or("finished"),
        );
    }
    if let Some(ws) = &task.workspace {
        let panes: Vec<String> = server.with_core(|c| {
            c.model
                .panes
                .iter()
                .filter(|x| &x.workspace == ws)
                .map(|x| x.id.clone())
                .collect()
        });
        for pid in panes {
            server.close_pane(&pid);
        }
    }
    let leases = vk_tasks::PortLeases::new(
        crate::paths::state_root(),
        vk_tasks::PortPool::parse("20000-29999", 10).map_err(|e| invalid(e.to_string()))?,
    );
    let _ = leases.release(&task.id);
    // The lease is gone, so are the task's previews (06 B2 lifecycle).
    crate::preview_fabric::retire_task_previews(server, &task.id);
    crate::sandbox::teardown(server, &task.id);
    let mut job = None;
    let kind = task.checkout.clone().unwrap_or_else(|| "worktree".into());
    if remove
        && kind != "none"
        && let Some(path) = task.worktree_path.clone()
    {
        let srv = server.clone();
        let id = task.id.clone();
        job = Some(format!("j{}", &ulid()[20..]));
        std::thread::spawn(move || {
            let j = vk_tasks::start_remove(
                Path::new(&path),
                vk_tasks::RemoveOptions {
                    force,
                    protect_dirty: true,
                    ..Default::default()
                },
            );
            let st = j.wait();
            let mut c = srv.core.lock().unwrap();
            let mut tx = Tx::new();
            tx.event(
                "worktree.removed",
                json!({"task": id, "path": path}),
                json!({"state": format!("{st:?}")}),
            );
            let _ = srv.commit(&mut c, tx);
        });
    }
    let mut c = server.core.lock().unwrap();
    let mut t2 = task.clone();
    t2.status = if p.get("archive").and_then(Value::as_bool).unwrap_or(remove) {
        "archived".into()
    } else {
        "finished".into()
    };
    let mut tx = Tx::new();
    tx.event(
        "task.status_changed",
        json!({"task": t2.id}),
        json!({"status": t2.status}),
    );
    tx.task(t2.clone());
    server.commit(&mut c, tx).map_err(internal)?;
    Ok(json!({"task": t2, "job": job}))
}

#[cfg(test)]
#[path = "run_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "run_auth_tests.rs"]
mod auth_tests;
