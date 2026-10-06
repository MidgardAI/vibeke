//! Server process: control socket, connection handling, housekeeping, task API.

use crate::api::{self, Ctx, R, err, internal, invalid, not_found, req, s};
use crate::core::{Tx, ulid};
use crate::{Server, render};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use vk_proto::model::Task;
use vk_proto::rpc::{ErrorKind, Request, Response};

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
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut n = 0u64;
        loop {
            tick.tick().await;
            hk.housekeeping();
            n += 1;
            if n.is_multiple_of(3600) {
                let _ = hk.with_core(|c| c.store.prune(7, 365));
            }
        }
    });
    crate::agents::start(&server);
    crate::sandbox::restore(&server).await;
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
        let _ = sd.ui.send(crate::UiEvent::Goodbye("server stopped".into()));
        tokio::time::sleep(Duration::from_millis(100)).await;
        std::process::exit(0);
    });
    loop {
        let (stream, _) = listener.accept().await?;
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
    let mut pid = pid? as u32;
    let roots: Vec<(u32, String)> = server.with_core(|c| {
        c.model
            .panes
            .iter()
            .filter_map(|p| p.child_pid.map(|cp| (cp, p.id.clone())))
            .collect()
    });
    if roots.is_empty() {
        return None;
    }
    for _ in 0..64 {
        if let Some((_, pane)) = roots.iter().find(|(cp, _)| *cp == pid) {
            return Some(pane.clone());
        }
        let info = vk_hold::procinfo::info(pid)?;
        if info.ppid <= 1 || info.ppid == pid {
            return None;
        }
        pid = info.ppid;
    }
    None
}

pub async fn connection<S>(server: Arc<Server>, stream: S, peer_pid: Option<i32>) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (rd, mut wr) = tokio::io::split(stream);
    let mut rd = BufReader::new(rd);
    let ancestry = ancestry_pane(&server, peer_pid);
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
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    let mut line = String::new();
    // Handle lines until render.attach (which needs the raw stream) or EOF.
    let attach = loop {
        tokio::select! {
            n = rd.read_line(&mut line) => {
                if n? == 0 { break None }
                let l = std::mem::take(&mut line);
                let l = l.trim_end_matches(['\n', '\r']);
                if l.is_empty() { continue }
                let Ok(req) = serde_json::from_str::<Request>(l) else {
                    let _ = out_tx.send(api::handle_line(&server, &ctx, l).await);
                    continue;
                };
                match req.method.as_str() {
                    "client.hello" => {
                        if let Some(tok) = req.params.get("token").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                            match server.pane_for_token(tok) {
                                // A token can't widen or switch scope away from the caller's own pane.
                                Some(p) if ancestry.as_ref().is_none_or(|a| *a == p) => ctx.pane_scope = Some(p),
                                _ => {
                                    let r = Response::err(req.id.clone().unwrap_or(Value::Null), err(ErrorKind::PermissionDenied, "unknown pane token"));
                                    let _ = out_tx.send(serde_json::to_string(&r)?);
                                    continue;
                                }
                            }
                        }
                        if let Some(k) = req.params.get("kind").and_then(Value::as_str) { ctx.kind = k.into(); }
                        if let Some(c) = req.params.get("client_id").and_then(Value::as_str) { ctx.client_id = c.into(); }
                        ctx.remote = req.params.get("remote").and_then(Value::as_bool).unwrap_or(false);
                        let _ = out_tx.send(api::handle_line(&server, &ctx, l).await);
                    }
                    "render.attach" => break Some(req),
                    "events.subscribe" => {
                        subscribe(&server, &req, out_tx.clone())?;
                    }
                    _ => {
                        let (srv, c, tx, l) = (server.clone(), ctx.clone(), out_tx.clone(), l.to_string());
                        tokio::spawn(async move { let _ = tx.send(api::handle_line(&srv, &c, &l).await); });
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
        if ctx.pane_scope.is_some() {
            let r = Response::err(
                req.id.unwrap_or(Value::Null),
                err(
                    ErrorKind::PermissionDenied,
                    "render.attach needs a user client",
                ),
            );
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
        let remote = req
            .params
            .get("remote")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let max_fps = req
            .params
            .get("caps")
            .and_then(|c| c.get("max_fps"))
            .and_then(Value::as_u64)
            .unwrap_or(120) as u32;
        let r = Response::ok(
            req.id.unwrap_or(Value::Null),
            json!({"protocol": vk_proto::render::PROTOCOL, "client_id": client_id}),
        );
        wr.write_all(serde_json::to_string(&r)?.as_bytes()).await?;
        wr.write_all(b"\n").await?;
        wr.flush().await?;
        render::serve(server, rd, wr, client_id, remote, max_fps).await?;
        return Ok(());
    }
    wr.flush().await?;
    Ok(())
}

/// `events.subscribe {after?, types?}`: backlog from the outbox, then live events; never silent
/// loss (overflow closes the subscription with `events.overflow`).
fn subscribe(
    server: &Arc<Server>,
    req: &Request,
    out: mpsc::UnboundedSender<String>,
) -> Result<()> {
    let id = req.id.clone().unwrap_or(Value::Null);
    let p = req.params.clone();
    let after = match api::after_seq(server, &p) {
        Ok(a) => a,
        Err(e) => {
            let _ = out.send(serde_json::to_string(&Response::err(id, e))?);
            return Ok(());
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
    let _ = out.send(serde_json::to_string(&Response::ok(
        id,
        json!({"subscription_id": sub_id, "at": at}),
    ))?);
    let srv = server.clone();
    tokio::spawn(async move {
        let notify = |e: &vk_store::Event| {
            serde_json::to_string(&json!({"jsonrpc": "2.0", "method": "events.event", "params": {"subscription_id": sub_id, "event": e}})).unwrap()
        };
        let mut last = after;
        if after > 0 || p.get("after").is_some() {
            loop {
                let batch = srv
                    .with_core(|c| c.store.events_after(last, 500, &types))
                    .unwrap_or_default();
                if batch.is_empty() {
                    break;
                }
                for e in &batch {
                    last = e.seq;
                    if out.send(notify(e)).is_err() {
                        return;
                    }
                }
            }
        } else {
            last = srv.with_core(|c| c.store.last_seq().unwrap_or(0));
        }
        loop {
            match rx.recv().await {
                Ok(e) => {
                    if e.seq <= last {
                        continue;
                    }
                    last = e.seq;
                    if !types.is_empty() && !types.iter().any(|g| vk_store::glob_match(g, &e.kind))
                    {
                        continue;
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
    Ok(())
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

fn repo_trusted(server: &Server, repo: &std::path::Path, digest: &str) -> bool {
    let key = repo
        .canonicalize()
        .unwrap_or_else(|_| repo.to_path_buf())
        .to_string_lossy()
        .into_owned();
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
    let Some(digest) = vibeke_dir_digest(&repo) else {
        return Err(invalid(format!(
            "{} has no .vibeke/ directory",
            repo.display()
        )));
    };
    if let Some(want) = s(p, "digest").filter(|d| *d != digest) {
        return Err(err(
            ErrorKind::Conflict,
            format!(".vibeke/ changed since review (expected {want}, now {digest})"),
        ));
    }
    let mut map = trust_map(server);
    map.insert(repo.to_string_lossy().into_owned(), digest.clone());
    let script = std::fs::read_to_string(repo.join(".vibeke/setup.sh")).ok();
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
        json!({"repo": repo, "digest": digest}),
    );
    server.commit(&mut c, tx).map_err(internal)?;
    Ok(json!({"repo": repo, "digest": digest, "setup_script": script}))
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
        m if m.starts_with("sandbox.")
            && let Some(r) = crate::sandbox::api(server, ctx, m, p).await =>
        {
            r
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
                    Ok(
                        json!({"task": task, "branch_status": status.map(|s| json!({"branch": s.branch, "ahead": s.ahead, "behind": s.behind, "dirty_files": s.dirty_files, "upstream": s.upstream, "compared_to": s.compared_to}))}),
                    )
                }
                None => Err(not_found("task", t)),
            }
        }
        "task.finish" => task_finish(server, p).await,
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
    let info = vk_tasks::repo_root(Path::new(&repo))
        .ok_or_else(|| invalid(format!("{repo} is not inside a git repository")))?;
    // Execution isolation (13 §3): validate before creating anything.
    let mut iso_req = crate::sandbox::IsoRequest::from_params(p, &crate::sandbox::load_cfg())?;
    if iso_req.level == vk_proto::model::IsolationLevel::Vm {
        return Err(err(
            ErrorKind::Unsupported,
            "the vm isolation level ships in M4; use --isolate sandbox",
        ));
    }
    let cfg = task_cfg(p);
    let creq = vk_tasks::CreateRequest {
        repo: info.root.clone(),
        title: title.clone(),
        base: s(p, "base").map(str::to_string),
        branch: s(p, "branch").map(str::to_string),
        slug: s(p, "slug").map(str::to_string),
    };
    let checkout = tokio::task::spawn_blocking(move || vk_tasks::create_worktree(&creq, &cfg))
        .await
        .map_err(internal)?
        .map_err(|e| err(ErrorKind::Conflict, e.to_string()))?;
    let copy = p
        .get("copy_files")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_else(vk_tasks::default_copy_files);
    let copied = vk_tasks::copy_files(&info.root, &checkout.path, &copy).unwrap_or_default();
    let id = ulid();
    let handle = server.with_core(|c| c.next_task_handle());
    // Port lease (machine-wide).
    let leases = vk_tasks::PortLeases::new(
        crate::paths::state_root(),
        vk_tasks::PortPool::parse(s(p, "port_pool").unwrap_or("20000-29999"), 10)
            .map_err(|e| invalid(e.to_string()))?,
    );
    let lease = leases
        .lease(&vk_tasks::LeaseRequest {
            task_id: id.clone(),
            session: server.opts.session.clone(),
            owner_pid: None,
        })
        .ok();
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
    let (ws, _tab, pane) = server
        .create_workspace(
            &cwd,
            Some(checkout.slug.clone()),
            None,
            p.get("focus")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                .then_some(ctx.client_id.as_str()),
        )
        .map_err(internal)?;
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
        server.commit(&mut c, tx).map_err(internal)?;
    }
    // Setup script in the background (05 §7).
    let script = checkout
        .path
        .join(s(p, "setup_script").unwrap_or(".vibeke/setup.sh"));
    // Repo automation runs only after trust (09 §4): (canonical repo path, digest of `.vibeke/`).
    let digest = vibeke_dir_digest(&checkout.path);
    let trusted = digest
        .as_ref()
        .is_some_and(|d| repo_trusted(server, &info.root, d));
    let wants_setup = p.get("setup").and_then(Value::as_bool).unwrap_or(true) && script.exists();
    if wants_setup && !trusted {
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        if let Some(mut t) = c.model.tasks.iter().find(|t| t.id == id).cloned() {
            t.setup_status = Some("untrusted".into());
            tx.task(t);
        }
        tx.event(
            "task.setup_untrusted",
            json!({"task": id}),
            json!({"repo": info.root, "digest": digest, "script": script, "hint": format!("review {} then run: vibeke policy trust {}", script.display(), info.root.display())}),
        );
        let _ = server.commit(&mut c, tx);
    }
    if wants_setup && trusted {
        let srv = server.clone();
        let task_id = id.clone();
        let wt = checkout.path.clone();
        let lease2 = lease.clone();
        std::thread::spawn(move || {
            let log = wt.join(".vibeke/setup.log");
            let opts = vk_tasks::SetupOptions {
                worktree: wt.clone(),
                script,
                task_id: task_id.clone(),
                lease: lease2,
                extra_env: vec![],
                log_path: log,
                timeout: Some(std::time::Duration::from_secs(600)),
            };
            let out = vk_tasks::run_setup(&opts, &vk_tasks::CancelToken::default());
            let status = match out {
                Ok(o) => format!("{:?}", o.status),
                Err(e) => format!("failed: {e}"),
            };
            let mut c = srv.core.lock().unwrap();
            if let Some(mut t) = c.task(&task_id).cloned() {
                t.setup_status = Some(status.clone());
                let mut tx = Tx::new();
                tx.event(
                    "task.setup_finished",
                    json!({"task": task_id}),
                    json!({"status": status}),
                );
                tx.task(t);
                let _ = srv.commit(&mut c, tx);
            }
        });
    }
    // Agents (`--agent claude:impl`).
    let mut runs = Vec::new();
    if let Some(agents) = p.get("agents").and_then(Value::as_array) {
        for a in agents {
            let harness = a.get("harness").and_then(Value::as_str).unwrap_or("claude");
            let name = a.get("name").and_then(Value::as_str);
            let prompt = a.get("prompt").and_then(Value::as_str);
            let opts = crate::sandbox::LaunchOpts {
                yolo: iso_req.yolo,
                isolate: Some(iso_req.level),
                network: None,
            };
            match crate::agents::start_in_pane_opts(
                server,
                &pane.id,
                harness,
                name,
                prompt,
                &[],
                Some(&id),
                &opts,
            )
            .await
            {
                Ok(r) => runs.push(r),
                Err(e) => return Err(e),
            }
        }
    }
    let copied: Vec<String> = copied
        .iter()
        .filter(|c| matches!(c.outcome, vk_tasks::CopyOutcome::Copied))
        .map(|c| c.rel.clone())
        .collect();
    Ok(
        json!({"task": task, "workspace": ws, "panes": [pane], "runs": runs, "copied": copied, "warnings": checkout.warnings}),
    )
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
    crate::sandbox::teardown(server, &task.id);
    let mut job = None;
    if remove && let Some(path) = task.worktree_path.clone() {
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
