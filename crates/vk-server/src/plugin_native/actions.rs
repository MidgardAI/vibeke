//! Native argv actions and `[[on]]` hooks (Kind A, 07 §7.2).
//!
//! * `plugin.action {plugin, action, context?}` waits and returns `{exit_code, stdout_tail,
//!   log}` (the last 4 KiB of stdout); `plugin.action.run` for a native plugin returns the
//!   running log record at once (the asynchronous service the TUI palette uses).
//! * An action with a `command` runs as argv (no shell) in the plugin dir with
//!   `VIBEKE_PLUGIN_TOKEN` (60 s), `VIBEKE_CONTEXT_WORKSPACE`/`VIBEKE_CONTEXT_PANE`; an action
//!   without one is sent to the running process (`plugin.action`).
//! * A non-zero exit raises a notification. Records go to the per-server ring and to
//!   `state.db` (`plugin_commands`); events `plugin.action_invoked` and
//!   `plugin.command_finished`.
//! * `[[on]] event = "…"` hooks get the event JSON on stdin (`VIBEKE_EVENT` names it); hooks
//!   share the plugin's concurrency limit (busy hooks are recorded as failed `busy`).

use super::tokens::{self, TokenInfo, TokenKind};
use super::{actor, emit, state};
use crate::Server;
use crate::api::{Ctx, R, err, invalid};
use serde_json::{Map, Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use vk_compat::native::manifest::Manifest;
use vk_compat::native::registry::{NativeEntry, NativeStatus, native_status};
use vk_proto::rpc::{ErrorKind, RpcError};

/// Records kept in memory per server.
const MAX_LOGS: usize = 200;
/// Bytes kept per stream (ring).
const KEEP: usize = 64 * 1024;
/// `stdout_tail` returned by `plugin.action` (07 §7.2).
const TAIL: usize = 4 * 1024;
/// Running argv invocations per plugin unless `[plugins] max_concurrent` says otherwise.
const DEFAULT_CONCURRENT: usize = 4;
/// How long a process-routed action may take.
const PROCESS_ACTION_TIMEOUT: Duration = Duration::from_secs(60);

/// `(plugin, action)` from `{plugin, action}` or a qualified `action: "<plugin>.<action>"`.
pub fn target(p: &Value) -> Result<(String, String), RpcError> {
    let s = |k: &str| p.get(k).and_then(Value::as_str);
    match (s("plugin"), s("action")) {
        (Some(pl), Some(a)) => Ok((pl.to_string(), a.to_string())),
        (None, Some(q)) => q
            .rsplit_once('.')
            .map(|(pl, a)| (pl.to_string(), a.to_string()))
            .ok_or_else(|| invalid("action must be <plugin>.<action>")),
        _ => Err(invalid("plugin and action are required")),
    }
}

fn max_concurrent(id: &str) -> usize {
    super::setting(id, "max_concurrent")
        .and_then(|v| v.as_integer())
        .map(|n| n.clamp(1, 64) as usize)
        .unwrap_or(DEFAULT_CONCURRENT)
}

/// Context of an invocation (07 §7.2 `VIBEKE_CONTEXT_*`).
#[derive(Debug, Clone, Default)]
pub struct InvokeCtx {
    pub workspace: Option<String>,
    pub pane: Option<String>,
    pub tab: Option<String>,
    pub source: String,
    pub extra: Map<String, Value>,
}

impl InvokeCtx {
    pub fn from_params(server: &Server, p: &Value) -> InvokeCtx {
        let c = p.get("context").cloned().unwrap_or(Value::Null);
        let pick = |k: &str| {
            c.get(k)
                .or_else(|| p.get(k))
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        let mut x = InvokeCtx {
            workspace: pick("workspace"),
            pane: pick("pane"),
            tab: pick("tab"),
            source: p
                .get("source")
                .and_then(Value::as_str)
                .filter(|s| matches!(*s, "palette" | "keybinding" | "status" | "sidebar" | "link"))
                .unwrap_or("cli")
                .to_string(),
            extra: Map::new(),
        };
        // Fill from the focus when the caller gave nothing.
        if x.pane.is_none() && x.workspace.is_none() {
            x.pane = server.focused_pane();
            if let Some(p) = &x.pane {
                server.with_core(|c| {
                    if let Some(pane) = c.pane(p) {
                        x.tab = Some(pane.tab.clone());
                        x.workspace = c
                            .model
                            .tabs
                            .iter()
                            .find(|t| t.id == pane.tab)
                            .map(|t| t.workspace.clone());
                    }
                });
            }
        }
        x
    }

    fn json(&self) -> Value {
        let mut v = json!({"workspace": self.workspace, "pane": self.pane, "tab": self.tab, "source": self.source});
        for (k, val) in &self.extra {
            v[k] = val.clone();
        }
        v
    }
}

// ---- records ---------------------------------------------------------------------------------

fn new_id() -> String {
    format!("pcmd-{}", &crate::core::ulid()[16..])
}

fn push_log(server: &Server, rec: &Value) {
    let st = state(server);
    let mut l = st.logs.lock().unwrap();
    l.push_back(rec.clone());
    while l.len() > MAX_LOGS {
        // Drop the oldest finished record.
        match l.iter().position(|r| r["status"] != "running") {
            Some(i) => {
                l.remove(i);
            }
            None => break,
        }
    }
    drop(l);
    persist(server, rec);
}

fn update_log(server: &Server, id: &str, f: impl FnOnce(&mut Map<String, Value>)) -> Option<Value> {
    let st = state(server);
    let mut l = st.logs.lock().unwrap();
    let r = l.iter_mut().find(|r| r["id"] == id)?;
    if let Some(m) = r.as_object_mut() {
        f(m);
    }
    let out = r.clone();
    drop(l);
    persist(server, &out);
    Some(out)
}

fn persist(server: &Server, rec: &Value) {
    let c = vk_store::PluginCommand {
        id: rec["id"].as_str().unwrap_or_default().to_string(),
        plugin_id: rec["plugin_id"].as_str().unwrap_or_default().to_string(),
        status: rec["status"].as_str().unwrap_or_default().to_string(),
        started_at: rec["started_at"].as_i64().unwrap_or(0),
        ended_at: rec["ended_at"].as_i64(),
        record: rec.clone(),
    };
    server.with_core(|core| {
        if let Err(e) = core.store.plugin_command_put(&c) {
            tracing::warn!(error = %e, "plugin command record not persisted");
        }
        let _ = core.store.plugin_commands_prune(50);
    });
}

/// Restore recent records after a server restart; records still `running` are marked
/// `unknown` (their process is not re-attached).
pub fn load_logs(server: &Server) {
    let rows = server
        .with_core(|c| c.store.plugin_commands(None, MAX_LOGS))
        .unwrap_or_default();
    let st = state(server);
    let mut l = st.logs.lock().unwrap();
    for r in rows.into_iter().rev() {
        let mut rec = r.record;
        if rec["status"] == "running" {
            rec["status"] = json!("unknown");
            rec["note"] = json!("server restarted while it ran");
        }
        l.push_back(rec);
    }
}

/// Native records, newest last; `plugin` filters.
pub fn logs(server: &Server, plugin: Option<&str>, limit: Option<u64>) -> Vec<Value> {
    let st = state(server);
    let l = st.logs.lock().unwrap();
    let v: Vec<Value> = l
        .iter()
        .filter(|r| plugin.is_none_or(|p| r["plugin_id"] == p))
        .cloned()
        .collect();
    let n = limit.unwrap_or(100) as usize;
    v[v.len().saturating_sub(n)..].to_vec()
}

fn tail_of(buf: &[u8], n: usize) -> String {
    let start = buf.len().saturating_sub(n);
    vk_redact::redact(&String::from_utf8_lossy(&buf[start..])).into_owned()
}

// ---- listing ---------------------------------------------------------------------------------

/// Native actions (manifest actions and contributed palette commands) in the
/// `plugin.action.list` shape.
pub fn list(server: &Server, plugin: Option<&str>) -> Vec<Value> {
    let reg = super::registry(server);
    let pf = vk_compat::herdr::current_platform();
    let mut out = vec![];
    for e in reg.native.values().filter(|e| plugin.is_none_or(|p| p == e.id)) {
        let (st, m) = native_status(e, super::vibeke_version());
        let Some(m) = m else { continue };
        let crashed = state(server).crash_disabled.lock().unwrap().contains(&e.id);
        let available = st == NativeStatus::Active && !crashed;
        for a in m.actions_on(pf) {
            out.push(json!({
                "plugin_id": e.id,
                "action_id": a.id,
                "qualified_id": format!("{}.{}", e.id, a.id),
                "title": a.title,
                "description": a.description,
                "contexts": a.contexts,
                "available": available,
                "status": if crashed { "crashed" } else { st.as_str() },
                "kind": "native",
            }));
        }
    }
    out.extend(super::ui::palette_actions(server, plugin));
    out
}

/// Manifest default key bindings and contributed `keybinding`s of native plugins, checked
/// against the user's keymap and the bindings already `claimed` (Herdr plugins first).
pub fn keybindings(server: &Server, mut claimed: Vec<(String, String)>) -> Vec<Value> {
    let cfg = vk_config::Config::load(vk_config::config_path())
        .map(|(c, _)| c)
        .unwrap_or_default();
    let reg = super::registry(server);
    let pf = vk_compat::herdr::current_platform();
    let mut want: Vec<(String, String, String, Option<String>, bool)> = vec![];
    for e in reg.native.values() {
        let (st, m) = native_status(e, super::vibeke_version());
        let Some(m) = m else { continue };
        let active = st == NativeStatus::Active;
        for a in m.actions_on(pf) {
            if let Some(k) = &a.keybinding {
                want.push((
                    e.id.clone(),
                    k.clone(),
                    format!("{}.{}", e.id, a.id),
                    Some(a.title.clone()),
                    active,
                ));
            }
        }
    }
    for (plugin, key, action) in super::ui::keybinding_contribs(server) {
        want.push((plugin, key, action, None, true));
    }
    let mut out = vec![];
    for (plugin, key, action, description, active) in want {
        let mut v = json!({
            "plugin_id": plugin,
            "key": key,
            "action": action,
            "description": description,
            "installed": false,
        });
        if !active {
            v["reason"] = json!("inactive");
        } else {
            match vk_config::binding_clash(&cfg, &key, &claimed) {
                Err(_) => v["reason"] = json!("invalid_key"),
                Ok(Some(other)) => {
                    v["reason"] = json!("conflict");
                    v["conflicts_with"] = json!(other);
                }
                Ok(None) => {
                    v["installed"] = json!(true);
                    claimed.push((action.clone(), key.clone()));
                }
            }
        }
        out.push(v);
    }
    out
}

// ---- invocation ------------------------------------------------------------------------------

/// `plugin.action` (wait) / `plugin.action.run` (async) for a native plugin.
pub async fn api_action(server: &Arc<Server>, ctx: &Ctx, p: &Value, wait: bool) -> R {
    let (plugin, action) = target(p)?;
    if ctx.pane_scope.is_some() {
        return Err(err(
            ErrorKind::PermissionDenied,
            "plugin actions are not available from a pane",
        ));
    }
    // A plugin may run its own actions only (no escalation through another plugin).
    if let Some(info) = super::caller(server, ctx)
        && info.plugin != plugin
    {
        return Err(err(
            ErrorKind::PermissionDenied,
            format!("{} may run its own actions only", info.plugin),
        ));
    }
    // A contributed pane's palette entry opens the pane.
    if let Some(pane) = super::ui::pane_for(&action) {
        if super::caller(server, ctx).is_some() {
            return Err(err(
                ErrorKind::PermissionDenied,
                "plugins cannot open their panes themselves",
            ));
        }
        let mut q = p.clone();
        q["plugin"] = json!(plugin);
        q["pane"] = json!(pane);
        return super::ui::open_pane(server, ctx, &q).map(|v| json!({"log": null, "pane": v["pane"]}));
    }
    let ictx = InvokeCtx::from_params(server, p);
    let (rec, done) = invoke(server, &plugin, &action, ictx).await?;
    if !wait {
        return Ok(json!({"log": rec}));
    }
    let fin = done.await.unwrap_or(rec);
    Ok(json!({
        "exit_code": fin["exit_code"],
        "stdout_tail": fin["stdout_tail"],
        "status": fin["status"],
        "log": fin,
    }))
}

/// Start an action; returns the running record and a receiver of the finished one.
pub async fn invoke(
    server: &Arc<Server>,
    plugin: &str,
    action: &str,
    ictx: InvokeCtx,
) -> Result<(Value, tokio::sync::oneshot::Receiver<Value>), RpcError> {
    let (e, m) = super::active(server, plugin)?;
    let pf = vk_compat::herdr::current_platform();
    // A contributed palette command maps to a manifest action of the same plugin.
    let action = super::ui::palette_target(server, plugin, action).unwrap_or(action.to_string());
    let Some(a) = m.action(&action, pf).cloned() else {
        return Err(err(
            ErrorKind::NotFound,
            format!("{plugin} has no action {action}"),
        )
        .details(json!({"object": "action"})));
    };
    vk_compat::native::registry::verify_launch(&e)
        .map_err(|why| err(ErrorKind::PermissionDenied, why))?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    let id = new_id();
    let rec = json!({
        "id": id,
        "plugin_id": plugin,
        "kind": "native",
        "source": ictx.source,
        "action_id": action,
        "context": ictx.json(),
        "status": "running",
        "started_at": super::now_ms(),
        "ended_at": null,
        "exit_code": null,
        "argv": a.command,
    });
    match a.command.clone() {
        None => {
            // Sent to the running process.
            push_log(server, &rec);
            emit_invoked(server, plugin, &action, &id, &ictx, "process");
            let (s, plugin, action, id, cjson) = (
                server.clone(),
                plugin.to_string(),
                action.clone(),
                id.clone(),
                ictx.json(),
            );
            tokio::spawn(async move {
                let r = super::process::request(
                    &s,
                    &plugin,
                    "plugin.action",
                    json!({"action": action, "context": cjson}),
                    PROCESS_ACTION_TIMEOUT,
                )
                .await;
                let (ok, out, msg) = match r {
                    Ok(v) => (true, v.to_string(), None),
                    Err(e) => (false, String::new(), Some(e.message)),
                };
                let fin = finish(&s, &plugin, &action, &id, ok, if ok { Some(0) } else { Some(1) }, out.as_bytes(), msg.as_deref().unwrap_or("").as_bytes(), msg.as_deref());
                let _ = tx.send(fin);
            });
            Ok((rec, rx))
        }
        Some(argv) => {
            let slot = acquire(server, plugin)?;
            push_log(server, &rec);
            emit_invoked(server, plugin, &action, &id, &ictx, "argv");
            let mut extra = vec![("VIBEKE_PLUGIN_ACTION_ID".to_string(), action.clone())];
            if let Some(w) = &ictx.workspace {
                extra.push(("VIBEKE_CONTEXT_WORKSPACE".into(), w.clone()));
            }
            if let Some(pn) = &ictx.pane {
                extra.push(("VIBEKE_CONTEXT_PANE".into(), pn.clone()));
            }
            extra.push(("VIBEKE_PLUGIN_CONTEXT_JSON".into(), ictx.json().to_string()));
            let (s, e2, m2, id2, act2) = (server.clone(), e.clone(), m.clone(), id.clone(), action.clone());
            tokio::spawn(async move {
                let fin = run_argv(&s, &e2, &m2, &argv, extra, None, &id2, &act2, TokenKind::Action).await;
                drop(slot);
                let _ = tx.send(fin);
            });
            Ok((rec, rx))
        }
    }
}

fn emit_invoked(server: &Server, plugin: &str, action: &str, id: &str, ictx: &InvokeCtx, how: &str) {
    emit(
        server,
        "plugin.action_invoked",
        json!({"plugin": plugin}),
        actor(plugin, Some(id)),
        json!({"action": action, "log": id, "source": ictx.source, "via": how}),
    );
}

/// A running-invocation slot (released on drop).
pub struct Slot {
    server: Arc<Server>,
    plugin: String,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let st = state(&self.server);
        let mut r = st.running.lock().unwrap();
        if let Some(n) = r.get_mut(&self.plugin) {
            *n = n.saturating_sub(1);
        }
    }
}

fn acquire(server: &Arc<Server>, plugin: &str) -> Result<Slot, RpcError> {
    let max = max_concurrent(plugin);
    let st = state(server);
    let mut r = st.running.lock().unwrap();
    let n = r.entry(plugin.to_string()).or_insert(0);
    if *n >= max {
        return Err(err(
            ErrorKind::Conflict,
            format!("busy: {plugin} already runs {max} commands"),
        )
        .details(json!({"reason": "busy"})));
    }
    *n += 1;
    Ok(Slot {
        server: server.clone(),
        plugin: plugin.to_string(),
    })
}

#[allow(clippy::too_many_arguments)]
fn finish(
    server: &Server,
    plugin: &str,
    what: &str,
    id: &str,
    ok: bool,
    code: Option<i32>,
    out: &[u8],
    errb: &[u8],
    note: Option<&str>,
) -> Value {
    let status = if ok { "completed" } else { "failed" };
    let fin = update_log(server, id, |m| {
        m.insert("status".into(), json!(status));
        m.insert("ended_at".into(), json!(super::now_ms()));
        m.insert("exit_code".into(), json!(code));
        m.insert("stdout_tail".into(), json!(tail_of(out, TAIL)));
        m.insert("stderr_tail".into(), json!(tail_of(errb, TAIL)));
        m.insert("stdout".into(), json!(tail_of(out, KEEP)));
        m.insert("stderr".into(), json!(tail_of(errb, KEEP)));
        if let Some(n) = note {
            m.insert("note".into(), json!(n));
        }
    })
    .unwrap_or_else(|| json!({"id": id, "status": status, "exit_code": code}));
    emit(
        server,
        "plugin.command_finished",
        json!({"plugin": plugin}),
        actor(plugin, Some(id)),
        json!({"log": id, "what": what, "status": status, "exit_code": code}),
    );
    if !ok {
        server.notify(
            "plugin",
            fin["context"]["pane"].as_str(),
            &format!("{plugin}: {what} failed"),
            &match (code, note) {
                (_, Some(n)) if !n.is_empty() => n.to_string(),
                (Some(c), _) => format!("exit status {c}"),
                _ => "failed".into(),
            },
            "normal",
        );
    }
    fin
}

/// Run one argv command with a short-lived token; `stdin` is written then closed.
#[allow(clippy::too_many_arguments)]
pub async fn run_argv(
    server: &Arc<Server>,
    e: &NativeEntry,
    m: &Manifest,
    argv: &[String],
    extra: Vec<(String, String)>,
    stdin: Option<Vec<u8>>,
    id: &str,
    what: &str,
    kind: TokenKind,
) -> Value {
    let consent = e.consent.clone().map(|c| c.consent_id).unwrap_or_default();
    let (token, tkind) = tokens::issue(
        server,
        TokenInfo {
            plugin: e.id.clone(),
            consent_id: consent,
            caps: m.capabilities.clone(),
            expires: Some(std::time::Instant::now() + tokens::SHORT_TTL),
            kind,
            invocation: id.to_string(),
        },
    );
    let launch = match super::launch::prepare(server, e, m, argv, &token, &extra, id).await {
        Ok(l) => l,
        Err(why) => {
            tokens::revoke_kind(server, &tkind);
            return finish(server, &e.id, what, id, false, None, b"", b"", Some(&why));
        }
    };
    let mut cmd = super::launch::command(&launch);
    cmd.stdin(if stdin.is_some() {
        std::process::Stdio::piped()
    } else {
        std::process::Stdio::null()
    })
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped());
    let spawned = cmd.spawn();
    if let Some(p) = &launch.profile {
        let _ = std::fs::remove_file(p);
    }
    let _proxy = launch.proxy;
    let mut child = match spawned {
        Ok(c) => c,
        Err(why) => {
            tokens::revoke_kind(server, &tkind);
            let msg = format!("{}: {why}", launch.argv[0]);
            return finish(server, &e.id, what, id, false, None, b"", b"", Some(&msg));
        }
    };
    if let (Some(data), Some(mut si)) = (stdin, child.stdin.take()) {
        tokio::spawn(async move {
            let _ = si.write_all(&data).await;
            let _ = si.shutdown().await;
        });
    }
    let read = |mut r: Box<dyn tokio::io::AsyncRead + Unpin + Send>| async move {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        while let Ok(n) = r.read(&mut chunk).await {
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.len() > KEEP * 2 {
                let cut = buf.len() - KEEP;
                buf.drain(..cut);
            }
        }
        buf
    };
    let out = tokio::spawn(read(Box::new(child.stdout.take().expect("piped"))));
    let er = tokio::spawn(read(Box::new(child.stderr.take().expect("piped"))));
    let status = child.wait().await.ok();
    let out = out.await.unwrap_or_default();
    let er = er.await.unwrap_or_default();
    tokens::revoke_kind(server, &tkind);
    let code = status.and_then(|s| s.code());
    finish(server, &e.id, what, id, code == Some(0), code, &out, &er, None)
}

// ---- [[on]] hooks ----------------------------------------------------------------------------

/// Run matching `[[on]]` hooks of active native plugins for every committed event.
pub async fn hook_dispatcher(server: Arc<Server>) {
    let mut rx = server.events.subscribe();
    loop {
        let ev = match rx.recv().await {
            Ok(e) => e,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(_) => return,
        };
        // Plugin bookkeeping events never trigger hooks (no loops through a hook's own
        // records).
        if ev.kind.starts_with("plugin.") || ev.kind.starts_with("ui.contributions") {
            continue;
        }
        let reg = super::registry(&server);
        for (e, m) in reg.native_active(super::vibeke_version()) {
            if m.on.is_empty() || state(&server).crash_disabled.lock().unwrap().contains(&e.id) {
                continue;
            }
            for h in &m.on {
                if !vk_compat::native::caps::event_matches(&h.event, &ev.kind)
                    || !m.capabilities.reads_event(&ev.kind)
                {
                    continue;
                }
                fire_hook(&server, &e, &m, h, &ev);
            }
        }
    }
}

fn fire_hook(
    server: &Arc<Server>,
    e: &NativeEntry,
    m: &Manifest,
    h: &vk_compat::native::manifest::Hook,
    ev: &vk_store::Event,
) {
    let id = new_id();
    let rec = json!({
        "id": id,
        "plugin_id": e.id,
        "kind": "native",
        "source": "event",
        "event": ev.kind,
        "status": "running",
        "started_at": super::now_ms(),
        "ended_at": null,
        "exit_code": null,
        "argv": h.command,
    });
    let slot = match acquire(server, &e.id) {
        Ok(s) => s,
        Err(_) => {
            let mut r = rec;
            r["status"] = json!("failed");
            r["note"] = json!("busy");
            r["ended_at"] = json!(super::now_ms());
            push_log(server, &r);
            return;
        }
    };
    push_log(server, &rec);
    let payload = serde_json::to_vec(ev).unwrap_or_default();
    let extra = vec![
        ("VIBEKE_EVENT".to_string(), ev.kind.clone()),
        ("VIBEKE_EVENT_SEQ".to_string(), ev.seq.to_string()),
    ];
    let (s, e2, m2, argv, what) = (
        server.clone(),
        e.clone(),
        m.clone(),
        h.command.clone(),
        format!("on {}", ev.kind),
    );
    tokio::spawn(async move {
        run_argv(&s, &e2, &m2, &argv, extra, Some(payload), &id, &what, TokenKind::Hook).await;
        drop(slot);
    });
}
