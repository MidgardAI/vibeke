//! Process plugins (Kind B, 07 §7.3): spawned and supervised by the server, speaking the same
//! JSON-RPC API over stdin/stdout (one JSON object per line, both directions).
//!
//! * Server → plugin: `plugin.initialize {config, api, plugin, capabilities}` (the reply's
//!   `contributions` are applied as `ui.contribute`), `plugin.action {action, context}` for
//!   manifest actions without a command, `plugin.shutdown` (5 s grace, then SIGTERM/SIGKILL of
//!   the process group) and `events.event {event}` notifications for events in `events_read`.
//! * Plugin → server: any API request, answered on stdin, dispatched with the plugin's
//!   capability-scoped identity (`client.hello` is not needed). The same token is in
//!   `VIBEKE_PLUGIN_TOKEN` for a socket connection.
//! * Exit: `restart = never | on-failure | always`; a failure restarts after 1, 2, 4, … s
//!   (≤ 60 s). More than 5 crashes in 10 minutes disables the plugin for this server with a
//!   notification (`plugin.crashed {disabled: true}`); `plugin.enable`/`plugin.restart` lift it.
//! * Server exit closes the plugin's stdin; a plugin is expected to exit on EOF.

use super::tokens::{self, TokenInfo, TokenKind};
use super::{actor, emit, state};
use crate::Server;
use crate::api::{R, err};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Notify, mpsc, oneshot};
use vk_compat::native::manifest::Restart;
use vk_proto::rpc::ErrorKind;

/// Crashes allowed in [`CRASH_WINDOW`] before the plugin is disabled.
pub const MAX_CRASHES: usize = 5;
pub const CRASH_WINDOW: Duration = Duration::from_secs(600);
/// `plugin.shutdown` grace (07 §7.3).
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
/// `plugin.initialize` must be answered within this.
pub const INIT_TIMEOUT: Duration = Duration::from_secs(10);
/// Bytes of stderr kept per incarnation.
const STDERR_KEEP: usize = 16 * 1024;
/// Longest accepted stdout line.
const MAX_LINE: usize = 4 * 1024 * 1024;

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, Value>>>>>;

/// One supervised plugin process (the slot exists from start until it is stopped for good).
#[derive(Clone)]
pub struct Handle {
    pub state: Arc<Mutex<String>>,
    pub pid: Arc<Mutex<Option<u32>>>,
    pub restarts: Arc<AtomicU64>,
    pub started_ms: Arc<Mutex<i64>>,
    pub tx: Arc<Mutex<Option<mpsc::UnboundedSender<String>>>>,
    pub pending: Pending,
    pub next_id: Arc<AtomicU64>,
    pub stop: Arc<Notify>,
    pub stopping: Arc<AtomicBool>,
    pub consent_id: String,
    pub stderr: Arc<Mutex<Vec<u8>>>,
}

impl Handle {
    fn new(consent_id: String) -> Handle {
        Handle {
            state: Arc::new(Mutex::new("starting".into())),
            pid: Arc::default(),
            restarts: Arc::default(),
            started_ms: Arc::default(),
            tx: Arc::default(),
            pending: Arc::default(),
            next_id: Arc::new(AtomicU64::new(1)),
            stop: Arc::new(Notify::new()),
            stopping: Arc::default(),
            consent_id,
            stderr: Arc::default(),
        }
    }

    fn set_state(&self, s: &str) {
        *self.state.lock().unwrap() = s.to_string();
    }

    /// Status for `plugin.list`.
    pub fn json(&self) -> Value {
        json!({
            "state": *self.state.lock().unwrap(),
            "pid": *self.pid.lock().unwrap(),
            "restarts": self.restarts.load(Ordering::Relaxed),
            "started_at": *self.started_ms.lock().unwrap(),
            "stderr_tail": String::from_utf8_lossy(&self.stderr.lock().unwrap()).into_owned(),
        })
    }
}

/// Start process plugins that should run (active, `[process]`, autostart, not crash-disabled)
/// and are not running yet.
pub fn ensure(server: &Arc<Server>) {
    let reg = super::registry(server);
    for (e, m) in reg.native_active(super::vibeke_version()) {
        let Some(p) = &m.process else { continue };
        if !p.autostart {
            continue;
        }
        if state(server).crash_disabled.lock().unwrap().contains(&e.id) {
            continue;
        }
        if state(server).procs.lock().unwrap().contains_key(&e.id) {
            continue;
        }
        start(server, &e.id);
    }
}

/// Start (supervise) plugin `id`'s process. No-op when already running.
pub fn start(server: &Arc<Server>, id: &str) -> bool {
    let consent = super::registry(server)
        .native
        .get(id)
        .and_then(|e| e.consent.as_ref().map(|c| c.consent_id.clone()))
        .unwrap_or_default();
    let h = {
        let st = state(server);
        let mut procs = st.procs.lock().unwrap();
        if procs.contains_key(id) {
            return false;
        }
        let h = Handle::new(consent);
        procs.insert(id.to_string(), h.clone());
        h
    };
    let (s, id) = (server.clone(), id.to_string());
    tokio::spawn(async move {
        supervise(&s, &id, h).await;
        state(&s).procs.lock().unwrap().remove(&id);
    });
    true
}

/// Stop plugin `id`'s process (shutdown with grace) and wait until it is gone (≤ 8 s).
pub async fn stop(server: &Server, id: &str, reason: &str) {
    let h = state(server).procs.lock().unwrap().get(id).cloned();
    let Some(h) = h else { return };
    tracing::info!(plugin = id, reason, "stopping plugin process");
    h.stopping.store(true, Ordering::SeqCst);
    h.stop.notify_waiters();
    h.stop.notify_one();
    let deadline = Instant::now() + SHUTDOWN_GRACE + Duration::from_secs(3);
    while Instant::now() < deadline {
        if !state(server).procs.lock().unwrap().contains_key(id) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// `plugin.restart {plugin}`: stop and start again (also lifts a crash-loop disable).
pub async fn restart(server: &Arc<Server>, id: &str) -> R {
    if id.is_empty() {
        return Err(crate::api::invalid("plugin is required"));
    }
    state(server).crash_disabled.lock().unwrap().remove(id);
    state(server).crashes.lock().unwrap().remove(id);
    let (_, m) = super::active(server, id)?;
    if m.process.is_none() {
        return Err(err(
            ErrorKind::InvalidParams,
            format!("{id} has no [process]"),
        ));
    }
    stop(server, id, "restart").await;
    start(server, id);
    Ok(json!({"plugin": id, "restarted": true}))
}

/// Send a request to plugin `id`'s process and wait for the answer.
pub async fn request(
    server: &Server,
    id: &str,
    method: &str,
    params: Value,
    timeout: Duration,
) -> R {
    let h = state(server)
        .procs
        .lock()
        .unwrap()
        .get(id)
        .cloned()
        .ok_or_else(|| err(ErrorKind::Conflict, format!("{id}'s process is not running")))?;
    send_request(&h, method, params, timeout).await
}

async fn send_request(h: &Handle, method: &str, params: Value, timeout: Duration) -> R {
    let tx = h
        .tx
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| err(ErrorKind::Conflict, "the plugin process is not running"))?;
    let n = h.next_id.fetch_add(1, Ordering::Relaxed);
    let (otx, orx) = oneshot::channel();
    h.pending.lock().unwrap().insert(n, otx);
    let line = json!({"jsonrpc": "2.0", "id": n, "method": method, "params": params}).to_string();
    if tx.send(line).is_err() {
        h.pending.lock().unwrap().remove(&n);
        return Err(err(ErrorKind::Conflict, "the plugin process is not running"));
    }
    match tokio::time::timeout(timeout, orx).await {
        Ok(Ok(Ok(v))) => Ok(v),
        Ok(Ok(Err(e))) => Err(err(
            ErrorKind::Internal,
            format!(
                "plugin error: {}",
                e.get("message").and_then(Value::as_str).unwrap_or("error")
            ),
        )
        .details(json!({"plugin_error": e}))),
        Ok(Err(_)) => Err(err(ErrorKind::Conflict, "the plugin process exited")),
        Err(_) => {
            h.pending.lock().unwrap().remove(&n);
            Err(err(ErrorKind::Timeout, format!("{method}: no answer from the plugin")))
        }
    }
}

fn notify_line(method: &str, params: Value) -> String {
    json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string()
}

/// Backoff before restart number `n` (1-based): 1, 2, 4, … s, at most 60 s.
pub fn backoff(n: usize) -> Duration {
    Duration::from_secs((1u64 << n.saturating_sub(1).min(6)).min(60))
}

/// Record a crash; true when the plugin must now be disabled (budget exhausted).
pub fn record_crash(server: &Server, id: &str) -> (usize, bool) {
    let st = state(server);
    let mut c = st.crashes.lock().unwrap();
    let v = c.entry(id.to_string()).or_default();
    let now = Instant::now();
    v.retain(|t| now.duration_since(*t) < CRASH_WINDOW);
    v.push(now);
    let n = v.len();
    (n, n > MAX_CRASHES)
}

fn kill_group(pid: u32, sig: libc::c_int) {
    // SAFETY: signalling the process group the child leads (setsid in pre_exec).
    unsafe {
        libc::kill(-(pid as i32), sig);
    }
}

async fn supervise(server: &Arc<Server>, id: &str, h: Handle) {
    loop {
        if h.stopping.load(Ordering::SeqCst) {
            break;
        }
        let (e, m) = match super::active(server, id) {
            Ok(x) => x,
            Err(e) => {
                tracing::info!(plugin = id, error = %e.message, "plugin process not started");
                break;
            }
        };
        let Some(pdecl) = m.process.clone() else { break };
        if let Err(why) = vk_compat::native::registry::verify_launch(&e) {
            super::launch::launch_failed(server, id, "process", &why);
            server.notify("plugin", None, &format!("{id} not started"), &why, "normal");
            break;
        }
        let consent = e.consent.clone().expect("active implies consent");
        let incarnation = format!("proc-{}", &crate::core::ulid()[18..]);
        let (token, kind) = tokens::issue(
            server,
            TokenInfo {
                plugin: id.to_string(),
                consent_id: consent.consent_id.clone(),
                caps: m.capabilities.clone(),
                expires: None,
                kind: TokenKind::Process,
                invocation: incarnation.clone(),
            },
        );
        let extra: Vec<(String, String)> = pdecl
            .env
            .iter()
            .filter(|(k, _)| !k.starts_with("VIBEKE_"))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let launch =
            match super::launch::prepare(server, &e, &m, &pdecl.command, &token, &extra, &incarnation)
                .await
            {
                Ok(l) => l,
                Err(why) => {
                    tokens::revoke_kind(server, &kind);
                    super::launch::launch_failed(server, id, "process", &why);
                    server.notify("plugin", None, &format!("{id} not started"), &why, "normal");
                    break;
                }
            };
        let mut cmd = super::launch::command(&launch);
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(why) => {
                tokens::revoke_kind(server, &kind);
                let msg = format!("{}: {why}", launch.argv[0]);
                super::launch::launch_failed(server, id, "process", &msg);
                if let Some(p) = &launch.profile {
                    let _ = std::fs::remove_file(p);
                }
                if handle_exit(server, id, &h, pdecl.restart, None, &msg).await {
                    continue;
                }
                break;
            }
        };
        if let Some(p) = &launch.profile {
            let _ = std::fs::remove_file(p);
        }
        let _proxy = launch.proxy;
        let pid = child.id().unwrap_or(0);
        *h.pid.lock().unwrap() = Some(pid);
        *h.started_ms.lock().unwrap() = super::now_ms();
        h.stderr.lock().unwrap().clear();
        h.set_state("running");
        // stdin writer.
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        *h.tx.lock().unwrap() = Some(tx.clone());
        let mut stdin = child.stdin.take().expect("piped");
        let writer = tokio::spawn(async move {
            while let Some(line) = rx.recv().await {
                if stdin.write_all(line.as_bytes()).await.is_err()
                    || stdin.write_all(b"\n").await.is_err()
                    || stdin.flush().await.is_err()
                {
                    break;
                }
            }
        });
        // stdout reader: plugin requests, notifications and answers.
        let stdout = child.stdout.take().expect("piped");
        let reader = {
            let (s, pending, tx, kind, plugin) = (
                server.clone(),
                h.pending.clone(),
                tx.clone(),
                kind.clone(),
                id.to_string(),
            );
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if line.len() > MAX_LINE || line.trim().is_empty() {
                        continue;
                    }
                    on_line(&s, &plugin, &kind, &pending, &tx, line);
                }
            })
        };
        let stderr_task = {
            let (mut se, buf) = (child.stderr.take().expect("piped"), h.stderr.clone());
            tokio::spawn(async move {
                let mut chunk = [0u8; 4096];
                while let Ok(n) = se.read(&mut chunk).await {
                    if n == 0 {
                        break;
                    }
                    let mut b = buf.lock().unwrap();
                    b.extend_from_slice(&chunk[..n]);
                    if b.len() > STDERR_KEEP {
                        let cut = b.len() - STDERR_KEEP;
                        b.drain(..cut);
                    }
                }
            })
        };
        // Events the plugin may read, pushed as notifications.
        let forwarder = {
            let (s, tx, caps, kind, plugin) = (
                server.clone(),
                tx.clone(),
                m.capabilities.clone(),
                kind.clone(),
                id.to_string(),
            );
            tokio::spawn(async move {
                if caps.events_read.is_empty() {
                    return;
                }
                let mut rx = s.events.subscribe();
                loop {
                    match rx.recv().await {
                        Ok(ev) => {
                            if !caps.reads_event(&ev.kind) {
                                continue;
                            }
                            // Never echo the plugin's own api_call/violation records back.
                            if ev.actor.get("id").and_then(Value::as_str) == Some(plugin.as_str())
                            {
                                continue;
                            }
                            if super::authorize_live(&s, &super::plugin_ctx(&plugin, &kind)).is_err() {
                                break;
                            }
                            let line = notify_line("events.event", json!({"event": &*ev}));
                            if tx.send(line).is_err() {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            let _ = tx.send(notify_line("events.lagged", json!({"missed": n})));
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        emit(
            server,
            "plugin.process_started",
            json!({"plugin": id}),
            actor(id, Some(&incarnation)),
            json!({"pid": pid, "sandbox": m.sandbox, "restarts": h.restarts.load(Ordering::Relaxed)}),
        );
        // Initialize (contributions) without blocking the exit watch.
        {
            let (s, h2, m2, plugin, consent_id) = (
                server.clone(),
                h.clone(),
                m.clone(),
                id.to_string(),
                consent.consent_id.clone(),
            );
            tokio::spawn(async move {
                let (data, config) = super::plugin_paths(&s, &plugin);
                let params = json!({
                    "config": super::setting(&plugin, "config").map(|v| serde_json::to_value(v).unwrap_or(Value::Null)),
                    "api": {
                        "version": vk_proto::VERSION,
                        "protocol": "jsonrpc-2.0-lines",
                        "methods": super::METHODS.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
                    },
                    "plugin": {"id": plugin, "version": m2.version, "data_dir": data, "config_dir": config},
                    "capabilities": m2.capabilities,
                    "session": s.paths.session,
                });
                match send_request(&h2, "plugin.initialize", params, INIT_TIMEOUT).await {
                    Ok(v) => {
                        if let Some(c) = v.get("contributions").and_then(Value::as_array) {
                            let r = super::ui::contribute(
                                &s,
                                &plugin,
                                &m2.capabilities,
                                &consent_id,
                                c,
                                true,
                                &[],
                            );
                            if let Err(e) = r {
                                tracing::warn!(plugin = %plugin, error = %e.message, "initialize contributions refused");
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(plugin = %plugin, error = %e.message, "plugin.initialize failed");
                    }
                }
            });
        }
        // Run until exit or stop.
        let status = tokio::select! {
            st = child.wait() => st.ok(),
            _ = h.stop.notified() => {
                // Graceful: plugin.shutdown, then the process group.
                let _ = send_request(&h, "plugin.shutdown", json!({}), SHUTDOWN_GRACE).await;
                match tokio::time::timeout(SHUTDOWN_GRACE, child.wait()).await {
                    Ok(st) => st.ok(),
                    Err(_) => {
                        kill_group(pid, libc::SIGTERM);
                        match tokio::time::timeout(Duration::from_secs(1), child.wait()).await {
                            Ok(st) => st.ok(),
                            Err(_) => {
                                kill_group(pid, libc::SIGKILL);
                                child.wait().await.ok()
                            }
                        }
                    }
                }
            }
        };
        *h.tx.lock().unwrap() = None;
        for (_, p) in h.pending.lock().unwrap().drain() {
            drop(p);
        }
        forwarder.abort();
        writer.abort();
        let _ = tokio::time::timeout(Duration::from_millis(500), reader).await;
        let _ = tokio::time::timeout(Duration::from_millis(500), stderr_task).await;
        tokens::revoke_kind(server, &kind);
        super::ui::clear(server, id);
        *h.pid.lock().unwrap() = None;
        let code = status.and_then(|s| s.code());
        let signal = status.and_then(|s| {
            use std::os::unix::process::ExitStatusExt;
            s.signal()
        });
        emit(
            server,
            "plugin.process_stopped",
            json!({"plugin": id}),
            actor(id, Some(&incarnation)),
            json!({"exit_code": code, "signal": signal, "requested": h.stopping.load(Ordering::SeqCst)}),
        );
        if h.stopping.load(Ordering::SeqCst) {
            break;
        }
        let ok = code == Some(0);
        let why = match (code, signal) {
            (Some(c), _) => format!("exited with status {c}"),
            (None, Some(sg)) => format!("killed by signal {sg}"),
            _ => "exited".into(),
        };
        if ok && pdecl.restart != Restart::Always {
            h.set_state("exited");
            break;
        }
        if ok {
            // `always`: a clean exit restarts without counting as a crash.
            h.restarts.fetch_add(1, Ordering::Relaxed);
            h.set_state("backoff");
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                _ = h.stop.notified() => break,
            }
            continue;
        }
        if !handle_exit(server, id, &h, pdecl.restart, code, &why).await {
            break;
        }
    }
    h.set_state("stopped");
}

/// After a failed run: crash accounting, `plugin.crashed`, backoff. True = restart now.
async fn handle_exit(
    server: &Server,
    id: &str,
    h: &Handle,
    restart: Restart,
    code: Option<i32>,
    why: &str,
) -> bool {
    let stderr_tail = {
        let b = h.stderr.lock().unwrap();
        let s = String::from_utf8_lossy(&b);
        let t: String = s.chars().rev().take(2048).collect::<Vec<_>>().into_iter().rev().collect();
        vk_redact::redact(&t).into_owned()
    };
    let (n, exhausted) = record_crash(server, id);
    let will_restart = restart != Restart::Never && !exhausted;
    let delay = backoff(n);
    emit(
        server,
        "plugin.crashed",
        json!({"plugin": id}),
        actor(id, None),
        json!({
            "exit_code": code,
            "reason": why,
            "crashes_in_window": n,
            "restart": will_restart,
            "restart_in_ms": will_restart.then_some(delay.as_millis() as u64),
            "disabled": exhausted,
            "stderr_tail": stderr_tail,
        }),
    );
    if exhausted {
        state(server).crash_disabled.lock().unwrap().insert(id.to_string());
        h.set_state("crashed");
        server.notify(
            "plugin",
            None,
            &format!("plugin {id} disabled"),
            &format!(
                "it crashed {n} times in 10 minutes ({why}); `vibeke plugin restart {id}` to try again"
            ),
            "high",
        );
        return false;
    }
    if !will_restart {
        h.set_state("crashed");
        server.notify("plugin", None, &format!("plugin {id} stopped"), why, "normal");
        return false;
    }
    h.restarts.fetch_add(1, Ordering::Relaxed);
    h.set_state("backoff");
    tokio::select! {
        _ = tokio::time::sleep(delay) => true,
        _ = h.stop.notified() => false,
    }
}

/// One line from the plugin's stdout.
fn on_line(
    server: &Arc<Server>,
    plugin: &str,
    kind: &str,
    pending: &Pending,
    tx: &mpsc::UnboundedSender<String>,
    line: String,
) {
    let Ok(v) = serde_json::from_str::<Value>(&line) else {
        tracing::debug!(plugin, "non-JSON line on a plugin's stdout ignored");
        return;
    };
    if v.get("method").is_some() {
        let is_request = v.get("id").is_some_and(|i| !i.is_null());
        let (s, ctx, tx) = (server.clone(), super::plugin_ctx(plugin, kind), tx.clone());
        tokio::spawn(async move {
            let resp = crate::api::handle_line(&s, &ctx, &line).await;
            if is_request {
                let _ = tx.send(resp);
            }
        });
        return;
    }
    let Some(id) = v.get("id").and_then(Value::as_u64) else {
        return;
    };
    let Some(waiter) = pending.lock().unwrap().remove(&id) else {
        return;
    };
    let r = match (v.get("result"), v.get("error")) {
        (_, Some(e)) if !e.is_null() => Err(e.clone()),
        (Some(r), _) => Ok(r.clone()),
        _ => Ok(Value::Null),
    };
    let _ = waiter.send(r);
}

/// Process status of plugin `id` (for `plugin.list`).
pub fn status(server: &Server, id: &str) -> Option<Value> {
    state(server).procs.lock().unwrap().get(id).map(|h| h.json())
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(backoff(1), Duration::from_secs(1));
        assert_eq!(backoff(2), Duration::from_secs(2));
        assert_eq!(backoff(4), Duration::from_secs(8));
        assert_eq!(backoff(20), Duration::from_secs(60));
    }
}
