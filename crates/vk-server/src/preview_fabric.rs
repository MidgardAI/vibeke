//! The rest of the preview fabric (06 B2, B4), server side:
//!
//! - **Reverse proxy** (B4): `preview.open {mode: "proxy"}` registers a per-preview origin on
//!   the viewing machine's proxy (`vk_preview::proxy`) and hands the user's normal browser a
//!   one-time tokenized URL. Upstreams are reached directly (this machine's loopback) or over
//!   this server's bridge link as `tcp:localhost:<port>` channels (remote previews).
//! - **Mirror** (B4): `preview.mirror` binds `127.0.0.1:<port>` (and `[::1]` when free) on the
//!   viewing machine and forwards raw TCP to the remote preview's port. Explicit, per preview,
//!   full scope only, never persisted. It cannot carry a credential; connections are
//!   peer-checked to belong to this user (other local users are refused).
//! - **Task previews** (B2 + 05 §6): `[previews]` from the task's repo config (and
//!   `task.create {previews}`) declared at task creation with ports from the task's lease.

use crate::Server;
use crate::api::{Ctx, R, b, err, invalid, not_found, s};
use crate::core::Tx;
use crate::preview::{
    PreviewConfig, commit_previews, find_local, is_local_machine, open_url_of, remote_call,
};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use vk_preview::proxy::{self, Proxy, Route};
use vk_preview::socks::{BoxFuture, Stream};
use vk_proto::model::*;
use vk_proto::rpc::{ErrorKind, RpcError};

// ---- reverse proxy ----------------------------------------------------------------------------

struct ServerUpstream {
    server: Weak<Server>,
}

impl proxy::Upstream for ServerUpstream {
    fn connect(&self, route: &Route) -> BoxFuture<std::io::Result<Box<dyn Stream>>> {
        let server = self.server.upgrade();
        let route = route.clone();
        Box::pin(async move {
            let server = server.ok_or_else(|| std::io::Error::other("server stopped"))?;
            connect_machine_port(&server, &route.machine, route.port).await
        })
    }
}

/// A stream to `localhost:<port>` on `machine`: direct for this machine, else a bridge `tcp:`
/// channel (the bridge refuses anything but loopback before connecting).
pub(crate) async fn connect_machine_port(
    server: &Server,
    machine: &str,
    port: u16,
) -> std::io::Result<Box<dyn Stream>> {
    if is_local_machine(server, machine) {
        let s = vk_remote::connect_loopback("localhost", port)
            .await
            .map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::ConnectionRefused, e.to_string())
            })?;
        return Ok(Box::new(s));
    }
    let link = server
        .previews
        .link(server, machine)
        .ok_or_else(|| std::io::Error::other(format!("no link to {machine}")))?;
    link.open_kind(&format!("tcp:localhost:{port}"))
        .await
        .map(|s| Box::new(s) as Box<dyn Stream>)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::ConnectionRefused, format!("{e:#}")))
}

/// Start the proxy once: `preview.proxy_port` (default 47800) on 127.0.0.1 (+ `[::1]`), or an
/// ephemeral port when that one is busy.
pub(crate) async fn ensure_proxy(server: &Arc<Server>, port: u16) -> std::io::Result<Arc<Proxy>> {
    let mut g = server.previews.proxy.lock().await;
    if let Some(p) = g.as_ref() {
        return Ok(p.clone());
    }
    let listeners = match Proxy::bind(port).await {
        Ok(l) => l,
        Err(e) if port != 0 => {
            tracing::warn!(port, error = %e, "preview proxy: configured port busy; using an ephemeral port");
            Proxy::bind(0).await?
        }
        Err(e) => return Err(e),
    };
    let p = Proxy::new(Arc::new(ServerUpstream {
        server: Arc::downgrade(server),
    }));
    p.serve(listeners);
    tracing::info!(port = p.port(), "preview proxy on 127.0.0.1");
    *g = Some(p.clone());
    Ok(p)
}

pub(crate) fn proxy_port(server: &Server) -> Option<u16> {
    server
        .previews
        .proxy
        .try_lock()
        .ok()
        .and_then(|g| g.as_ref().map(|p| p.port()))
}

pub(crate) async fn proxy_status(server: &Server) -> Value {
    let g = server.previews.proxy.lock().await;
    match g.as_ref() {
        None => Value::Null,
        Some(p) => json!({
            "port": p.port(),
            "routes": p.routes(),
            "stats": p.stats(),
        }),
    }
}

pub(crate) async fn forget_route(server: &Server, machine: &str, preview: &str) {
    if let Some(p) = server.previews.proxy.lock().await.as_ref() {
        p.remove_preview(machine, preview);
    }
}

fn machine_label(server: &Server, machine: &str) -> String {
    if is_local_machine(server, machine) {
        "local".into()
    } else {
        machine.to_string()
    }
}

/// `proxy_url` for `preview.url` (no side effects: only an origin that already exists).
pub(crate) async fn existing_proxy_url(
    server: &Server,
    machine: &str,
    pv: &Preview,
) -> Option<String> {
    let label = machine_label(server, machine);
    let g = server.previews.proxy.lock().await;
    let p = g.as_ref()?;
    p.routes()
        .into_iter()
        .find(|r| r.machine == label && r.preview == pv.id)
        .map(|r| p.url(&r.host, &pv.path, None))
}

/// Run the platform opener on `url` (the user's default browser). `VIBEKE_NO_OPEN=1` (tests,
/// headless use) skips it.
fn open_in_default_browser(url: &str) -> Result<bool, RpcError> {
    if std::env::var_os("VIBEKE_NO_OPEN").is_some() {
        return Ok(false);
    }
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(opener)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| err(ErrorKind::Unsupported, format!("{opener}: {e}")))?;
    Ok(true)
}

/// `preview.open {preview, mode: "proxy" | proxy: true, no_open?}` (B4).
///
/// Result: `{opened_in: "proxy", url, proxy_url, host, proxy_port, machine, preview, opened,
/// token_ttl_s, open_url?}`. `open_url` (the one-time tokenized URL) is returned only to
/// full-scope callers: an agent never receives a credential for the user's browser.
pub(crate) async fn open_proxy(
    server: &Arc<Server>,
    ctx: &Ctx,
    p: &Value,
    cfg: &PreviewConfig,
    machine: &str,
    pv: &Preview,
) -> R {
    let label = machine_label(server, machine);
    let local = label == "local";
    let slug = if local {
        pv.task
            .as_ref()
            .and_then(|t| server.with_core(|c| c.task(t).map(|x| x.slug.clone())))
    } else {
        None
    }
    .or_else(|| pv.label.clone());
    let wanted = proxy::hostname_for(
        &pv.handle,
        (!local).then_some(label.as_str()),
        slug.as_deref(),
    );
    let proxy = ensure_proxy(server, cfg.proxy_port)
        .await
        .map_err(|e| err(ErrorKind::Internal, format!("preview proxy: {e}")))?;
    let route = proxy.register(Route {
        host: wanted,
        machine: label.clone(),
        preview: pv.id.clone(),
        handle: pv.handle.clone(),
        port: pv.port,
        scheme: if pv.scheme == "https" {
            "https"
        } else {
            "http"
        }
        .into(),
    });
    let token = proxy
        .mint_token(&route.host)
        .ok_or_else(|| err(ErrorKind::Internal, "preview proxy: route vanished"))?;
    let plain = proxy.url(&route.host, &pv.path, None);
    let open_url = proxy.url(&route.host, &pv.path, Some(&token));
    let full_scope = ctx.pane_scope.is_none();
    // `no_open: true` (API) / `open: false` (CLI `--no-open`): print the link instead.
    let no_open = full_scope && (b(p, "no_open").unwrap_or(false) || b(p, "open") == Some(false));
    let opened = if no_open {
        false
    } else {
        open_in_default_browser(&open_url)?
    };
    {
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        let subj = if local {
            json!({"preview": pv.id, "preview_handle": pv.handle, "pane": pv.pane, "task": pv.task, "machine": label})
        } else {
            json!({"machine": label, "preview_handle": pv.handle, "preview": pv.id})
        };
        // Never the token: events are persisted and readable by agents.
        tx.event(
            "preview.opened",
            subj,
            json!({"url": plain, "opened_in": "proxy", "host": route.host}),
        );
        let _ = server.commit(&mut c, tx);
    }
    let mut out = json!({
        "opened_in": "proxy",
        "url": plain,
        "proxy_url": plain,
        "host": route.host,
        "proxy_port": proxy.port(),
        "machine": label,
        "preview": pv.handle,
        "opened": opened,
        "token_ttl_s": proxy::TOKEN_TTL.as_secs(),
        "remote_url": pv.url,
        "caveats": "Proxy mode rewrites Host/Origin; HMR clients with a hard-coded host/clientPort bypass the proxy, cookies for plain localhost don't apply. Prefer the browser profile (pane/window).",
    });
    if full_scope {
        out["open_url"] = json!(open_url);
    }
    Ok(out)
}

// ---- mirror -----------------------------------------------------------------------------------

/// A live mirror: aborting the accept loops drops their connection tasks too.
pub(crate) struct Mirror {
    machine: String,
    preview: String,
    handle: String,
    port: u16,
    addrs: Vec<SocketAddr>,
    since_ms: i64,
    accepted: Arc<AtomicU64>,
    rejected: Arc<AtomicU64>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Mirror {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

impl Mirror {
    fn json(&self) -> Value {
        json!({
            "machine": self.machine,
            "preview": self.preview,
            "preview_handle": self.handle,
            "local_port": self.port,
            "addrs": self.addrs.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
            "since_ms": self.since_ms,
            "accepted": self.accepted.load(Ordering::Relaxed),
            "rejected": self.rejected.load(Ordering::Relaxed),
            "authenticated": false,
            "peer_check": "same_user",
        })
    }
}

pub(crate) fn mirrors_status(server: &Server) -> Value {
    let m = server.previews.mirrors.lock().unwrap();
    let mut v: Vec<Value> = m.values().map(Mirror::json).collect();
    v.sort_by_key(|x| x["local_port"].as_u64());
    json!(v)
}

const MIRROR_WARNING: &str = "Unauthenticated raw port on this machine's loopback (connections from other local users are refused, any process of yours can connect). Prefer the browser profile (pane/window) or --proxy.";

fn full_scope_only(ctx: &Ctx, method: &str) -> Result<(), RpcError> {
    if ctx.pane_scope.is_some() {
        return Err(err(
            ErrorKind::PermissionDenied,
            format!("{method} is not allowed from a pane"),
        )
        .details(json!({"scope": "pane"})));
    }
    Ok(())
}

async fn mirror_loop(
    server: Weak<Server>,
    l: tokio::net::TcpListener,
    machine: String,
    port: u16,
    accepted: Arc<AtomicU64>,
    rejected: Arc<AtomicU64>,
) {
    let local = l.local_addr().ok();
    let mut conns = tokio::task::JoinSet::new();
    loop {
        let (s, peer) = tokio::select! {
            r = l.accept() => match r {
                Ok(x) => x,
                Err(_) => {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    continue;
                }
            },
            Some(_) = conns.join_next(), if !conns.is_empty() => continue,
        };
        let Some(local) = local else { continue };
        if !peer.ip().to_canonical().is_loopback() {
            continue;
        }
        let Some(server) = server.upgrade() else {
            return;
        };
        let machine = machine.clone();
        let (accepted, rejected) = (accepted.clone(), rejected.clone());
        conns.spawn(async move {
            // Peer check: the connecting socket must belong to a process of this user.
            let mine = tokio::task::spawn_blocking(move || {
                vk_preview::sockets::connection_owned_by_me(peer, local)
            })
            .await
            .unwrap_or(false);
            if !mine {
                rejected.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(%peer, port, "preview mirror: refused a connection from another user");
                return;
            }
            accepted.fetch_add(1, Ordering::Relaxed);
            let _ = s.set_nodelay(true);
            match connect_machine_port(&server, &machine, port).await {
                Ok(mut up) => {
                    let mut s = s;
                    let _ = tokio::io::copy_bidirectional(&mut s, &mut up).await;
                }
                Err(e) => {
                    tracing::debug!(%machine, port, error = %e, "preview mirror: upstream failed");
                }
            }
        });
    }
}

/// Resolve `{preview: "devbox/v4" | "v4", machine?}` to (machine label, remote preview).
async fn remote_preview(server: &Arc<Server>, p: &Value) -> Result<(String, Preview), RpcError> {
    let t = s(p, "preview").ok_or_else(|| invalid("missing param `preview`"))?;
    let (m, t) = match t.split_once('/') {
        Some((m, t)) if !m.is_empty() && !t.is_empty() => (m.to_string(), t.to_string()),
        _ => (s(p, "machine").unwrap_or("").to_string(), t.to_string()),
    };
    if is_local_machine(server, &m) {
        let pv = find_local(server, &t)?;
        return Err(invalid(format!(
            "preview {} is on this machine (localhost:{}); mirroring is for remote previews",
            pv.handle, pv.port
        )));
    }
    let v = remote_call(server, &m, "preview.get", json!({"preview": t})).await?;
    let pv: Preview = serde_json::from_value(v["preview"].clone())
        .map_err(|e| invalid(format!("remote preview: {e}")))?;
    Ok((m, pv))
}

/// `preview.mirror {preview}` → `{local_port, machine, preview, url, warning, addrs}`.
pub(crate) async fn mirror(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    full_scope_only(ctx, "preview.mirror")?;
    let (machine, pv) = remote_preview(server, p).await?;
    let port = pv.port;
    if let Some(m) = server.previews.mirrors.lock().unwrap().get(&port) {
        if m.machine == machine && m.preview == pv.id {
            let mut v = m.json();
            v["already"] = json!(true);
            v["warning"] = json!(MIRROR_WARNING);
            return Ok(v);
        }
        return Err(err(
            ErrorKind::Conflict,
            format!(
                "local port {port} already mirrors {}/{}; only one mirror per port",
                m.machine, m.handle
            ),
        ));
    }
    let v4 = match tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            return Err(err(
                ErrorKind::Conflict,
                format!(
                    "port {port} is busy on this machine; cannot mirror {machine}/{}",
                    pv.handle
                ),
            )
            .details(json!({"port": port, "machine": machine, "preview": pv.handle})));
        }
        Err(e) => {
            return Err(err(
                ErrorKind::Internal,
                format!("bind 127.0.0.1:{port}: {e}"),
            ));
        }
    };
    let mut listeners = vec![v4];
    // `localhost` may resolve to ::1 first; mirror it too when free (best effort).
    if let Ok(l6) = tokio::net::TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, port)).await {
        listeners.push(l6);
    }
    let addrs: Vec<SocketAddr> = listeners
        .iter()
        .filter_map(|l| l.local_addr().ok())
        .collect();
    let accepted = Arc::new(AtomicU64::new(0));
    let rejected = Arc::new(AtomicU64::new(0));
    let tasks = listeners
        .into_iter()
        .map(|l| {
            tokio::spawn(mirror_loop(
                Arc::downgrade(server),
                l,
                machine.clone(),
                port,
                accepted.clone(),
                rejected.clone(),
            ))
        })
        .collect();
    let m = Mirror {
        machine: machine.clone(),
        preview: pv.id.clone(),
        handle: pv.handle.clone(),
        port,
        addrs,
        since_ms: server.previews.now(),
        accepted,
        rejected,
        tasks,
    };
    let mut out = m.json();
    server.previews.mirrors.lock().unwrap().insert(port, m);
    let url = open_url_of(&pv);
    {
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.event(
            "preview.mirrored",
            json!({"machine": machine, "preview": pv.id, "preview_handle": pv.handle}),
            json!({"local_port": port, "url": url, "warning": MIRROR_WARNING}),
        );
        let _ = server.commit(&mut c, tx);
    }
    tracing::warn!(%machine, port, preview = %pv.handle, "preview mirror enabled (unauthenticated loopback port)");
    out["url"] = json!(url);
    out["warning"] = json!(MIRROR_WARNING);
    Ok(out)
}

/// `preview.unmirror {preview | port}` → `{local_port, machine, preview}`.
pub(crate) fn unmirror(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    full_scope_only(ctx, "preview.unmirror")?;
    // `vibeke preview unmirror 5173` passes the port as a number.
    let want_port = crate::api::u(p, "port")
        .or_else(|| crate::api::u(p, "preview"))
        .and_then(|x| u16::try_from(x).ok());
    let target = s(p, "preview").map(|t| match t.split_once('/') {
        Some((m, t)) if !m.is_empty() && !t.is_empty() => (Some(m.to_string()), t.to_string()),
        _ => (s(p, "machine").map(str::to_string), t.to_string()),
    });
    let removed = {
        let mut ms = server.previews.mirrors.lock().unwrap();
        let key = ms
            .iter()
            .find(|(port, m)| {
                want_port.is_some_and(|w| w == **port)
                    || target.as_ref().is_some_and(|(mach, t)| {
                        (m.handle == *t || m.preview == *t || t.parse::<u16>().ok() == Some(**port))
                            && mach.as_ref().is_none_or(|x| *x == m.machine)
                    })
            })
            .map(|(k, _)| *k);
        key.and_then(|k| ms.remove(&k))
    };
    let m = removed.ok_or_else(|| {
        not_found(
            "preview",
            s(p, "preview").unwrap_or("(no mirror on that port)"),
        )
    })?;
    let out = json!({"local_port": m.port, "machine": m.machine, "preview": m.handle});
    {
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.event(
            "preview.unmirrored",
            json!({"machine": m.machine, "preview": m.preview, "preview_handle": m.handle}),
            json!({"local_port": m.port}),
        );
        let _ = server.commit(&mut c, tx);
    }
    drop(m); // aborts the accept loops and their connections
    Ok(out)
}

/// Env a task's panes get so a hand-started dev server uses the leased ports: `PORT` and the
/// `[ports] env` names of the checkout's repo config (offsets into the task's lease).
pub(crate) fn task_port_env(task: &Task) -> Vec<(String, String)> {
    let Some((start, end)) = task.port_range else {
        return vec![];
    };
    let ports = task
        .worktree_path
        .as_deref()
        .and_then(|p| repo_preview_config(Path::new(p)).1);
    let mut env: Vec<(String, String)> = Vec::new();
    for (name, off) in vk_tasks::port_env_offsets(ports.as_ref()) {
        // Only names a shell can export, and only offsets inside the lease.
        let ok = !name.is_empty()
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !name.starts_with(|c: char| c.is_ascii_digit());
        let port = u32::from(start) + u32::from(off);
        if ok && port <= u32::from(end) {
            env.retain(|(k, _)| k != &name);
            env.push((name, port.to_string()));
        }
    }
    env
}

// ---- task previews ----------------------------------------------------------------------------

fn toml_json(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    match toml::from_str::<toml::Value>(&text) {
        Ok(v) => serde_json::to_value(v).ok(),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "task previews: unreadable TOML");
            None
        }
    }
}

/// `[previews]` and `[ports]` from the checkout's repo config: `.vibeke/task.toml`,
/// `.vibeke/previews.toml` (the whole file is the table) and `.vibeke/config.toml`
/// (`[previews]`/`[task.previews]`, `[ports]`/`[task.ports]`). First definition of a name wins.
fn repo_preview_config(checkout: &Path) -> (Value, Option<Value>) {
    let dir = checkout.join(".vibeke");
    let mut previews = serde_json::Map::new();
    let mut ports: Option<Value> = None;
    let mut merge = |t: Option<&Value>| {
        if let Some(o) = t.and_then(Value::as_object) {
            for (k, v) in o {
                previews.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
    };
    let task = toml_json(&dir.join("task.toml"));
    let pv = toml_json(&dir.join("previews.toml"));
    let cfg = toml_json(&dir.join("config.toml"));
    merge(task.as_ref().and_then(|t| t.get("previews")));
    merge(pv.as_ref());
    merge(cfg.as_ref().and_then(|c| c.get("previews")));
    merge(cfg.as_ref().and_then(|c| c.pointer("/task/previews")));
    for src in [
        task.as_ref().and_then(|t| t.get("ports")),
        cfg.as_ref().and_then(|c| c.get("ports")),
        cfg.as_ref().and_then(|c| c.pointer("/task/ports")),
    ] {
        if ports.is_none() && src.is_some() {
            ports = src.cloned();
        }
    }
    (Value::Object(previews), ports)
}

/// Declare a new task's previews (06 B2) from repo config (leased ports only) and
/// `task.create {previews}` (absolute ports allowed: the caller's own config). Returns
/// `{previews: [preview…], warnings: [..]}`.
pub(crate) fn declare_task_previews(
    server: &Arc<Server>,
    ctx: &Ctx,
    task: &Task,
    pane: &str,
    checkout: &Path,
    lease: Option<&vk_tasks::Lease>,
    p: &Value,
) -> Value {
    let (repo, ports) = repo_preview_config(checkout);
    let offsets = vk_tasks::port_env_offsets(ports.as_ref());
    let (repo_specs, mut warnings) = vk_tasks::parse_previews(&repo);
    let (mut resolved, w) = vk_tasks::resolve_previews(&repo_specs, lease, &offsets, false);
    warnings.extend(w);
    if let Some(user) = p.get("previews").filter(|v| !v.is_null()) {
        let (specs, w) = vk_tasks::parse_previews(user);
        warnings.extend(w);
        let (r, w) = vk_tasks::resolve_previews(&specs, lease, &offsets, true);
        warnings.extend(w);
        for x in r {
            // The caller's definition replaces the repo's of the same name.
            resolved.retain(|y| y.name != x.name && y.port != x.port);
            resolved.push(x);
        }
    }
    resolved.truncate(vk_tasks::MAX_TASK_PREVIEWS);
    let mut declared = Vec::new();
    for r in resolved {
        let params = json!({
            "port": r.port,
            "path": r.path,
            "label": r.label,
            "scheme": r.scheme,
            "task": task.id,
            "pane": pane,
        });
        match crate::preview::declare(server, ctx, &params) {
            Ok(v) => {
                let mut pv = v["preview"].clone();
                pv["name"] = json!(r.name);
                pv["from"] = json!(r.from);
                declared.push(pv);
            }
            Err(e) => warnings.push(format!("preview {}: {}", r.name, e.message)),
        }
    }
    for w in &warnings {
        tracing::warn!(task = %task.handle, "task previews: {w}");
    }
    json!({"previews": declared, "warnings": warnings})
}

/// Task finished/removed: its previews are gone (B2 lifecycle) and lose their proxy origins.
pub(crate) fn retire_task_previews(server: &Arc<Server>, task_id: &str) {
    let mine: Vec<Preview> = server.with_core(|c| {
        c.model
            .previews
            .iter()
            .filter(|p| p.task.as_deref() == Some(task_id) && p.status != PreviewStatus::Gone)
            .cloned()
            .collect()
    });
    if mine.is_empty() {
        return;
    }
    if let Ok(g) = server.previews.proxy.try_lock()
        && let Some(px) = g.as_ref()
    {
        for p in &mine {
            px.remove_preview("local", &p.id);
        }
    }
    let items = mine
        .into_iter()
        .map(|mut p| {
            vk_preview::lifecycle::retire(&mut p);
            (p, Some("preview.gone"))
        })
        .collect();
    commit_previews(server, items);
}

#[cfg(test)]
#[path = "preview_fabric_tests.rs"]
mod tests;
