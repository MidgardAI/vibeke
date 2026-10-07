//! Spec 13 lifecycle and UX extras on top of `sandbox.rs` (lane 2E):
//!
//! - `isolation.confirm_host_yolo` (§3): a Vibeke-launched `--yolo --isolate host` needs a
//!   one-time confirmation per workspace (repo root), recorded in kv.
//! - `isolation.suggest_sandbox_for_yolo` (§3): a user-typed host yolo run with a resume handle
//!   gets one `sandbox.suggested` nudge; `sandbox.relaunch` restarts it from its session inside
//!   an isolation level.
//! - Persistent global "allow always" egress approvals (§7), applied to every live proxy.
//! - The first-use credential notice and `sandbox.boundary_action {kind: credential_use}` (§8).
//! - Crash handling (§11): a container box that disappears while its task runs ends the task's
//!   runs with `exited{reason: runner_lost}`, cancels their interactions and records them for
//!   `sandbox.recover` (fresh box from the template/synced branch, runs resumed).
//! - Idle suspend (§11): boxes with no working run, no pending interaction and no input for
//!   `isolation.idle_suspend` are paused (processes kept) and thawed on input, a new pane,
//!   `sandbox.start` or `task.resume`.
//! - `sandbox.resource_pressure` (§11) from periodic `stats` samples against the box limits.
//! - `vibeke sandbox shell|logs|prune` (§11) and the Claude `setup-token` store (§8).

use super::*;
use std::collections::HashSet;
use vk_sandbox::boxops;
use vk_sandbox::container::BoxState;

const KV_GLOBAL: &str = "sandbox_egress";
const KV_YOLO: &str = "sandbox_yolo_host";
const KV_NOTICE: &str = "sandbox_notice";
const KV_LOST: &str = "sandbox_lost";
/// One pressure event per box and resource per this long.
const PRESSURE_EVERY: Duration = Duration::from_secs(300);
/// How often container box state is checked (crash detection, idle suspend).
const STATE_POLL: Duration = Duration::from_secs(10);
/// How often listening ports inside linked boxes are scanned.
const PORTS_POLL: Duration = Duration::from_secs(5);

#[derive(Default)]
pub struct ExState {
    inner: Mutex<ExInner>,
}

#[derive(Default)]
struct ExInner {
    /// Test override for the `[isolation]` config.
    cfg: Option<IsolationConfig>,
    watch: HashMap<String, BoxWatch>,
    polling: bool,
    last_state_poll: Option<Instant>,
    last_stats_poll: Option<Instant>,
    last_ports_poll: Option<Instant>,
    pressure_sent: HashMap<(String, &'static str), Instant>,
    /// Boxes paused by idle suspend.
    paused: HashSet<String>,
    /// Boxes Vibeke itself stopped or removed (not a crash), with when that was decided: a
    /// `Running` state read that began before then (a poll racing the stop) does not cancel it.
    expected_down: HashMap<String, Instant>,
    /// Runs already nudged towards a sandbox.
    suggested: HashSet<String>,
    listening: bool,
    /// Last resource sample per box (for `sandbox.list`).
    usage: HashMap<String, Value>,
}

#[derive(Default)]
struct BoxWatch {
    seen_running: bool,
    last_running_ms: i64,
    idle_since: Option<Instant>,
}

/// The `[isolation]` config (test override first).
pub fn cfg(server: &Server) -> IsolationConfig {
    if let Some(c) = server.sandbox.extras.inner.lock().unwrap().cfg.clone() {
        return c;
    }
    load_cfg()
}

/// Use `c` instead of the config file (tests).
pub fn set_cfg(server: &Server, c: IsolationConfig) {
    server.sandbox.extras.inner.lock().unwrap().cfg = Some(c);
}

fn kv_get(server: &Server, ns: &str, key: &str) -> Option<String> {
    server.with_core(|c| c.store.kv_get(ns, key).ok().flatten())
}

fn kv_set(server: &Server, ns: &str, key: &str, v: Option<String>) {
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.m.kv(ns, key, v);
    let _ = server.commit(&mut c, tx);
}

// ---- confirm_host_yolo (13 §3) -------------------------------------------------------------------

/// The workspace a host-yolo confirmation covers: the main repo root of `path` (or the path).
pub fn workspace_key(path: &Path) -> String {
    vk_tasks::repo_root(path)
        .map(|r| r.root)
        .unwrap_or_else(|| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// Vibeke is about to launch a yolo run on the host for workspace `key_path`: refuse unless the
/// workspace was confirmed before, the caller confirms now (`confirm`, recorded), or
/// `isolation.confirm_host_yolo = false`.
pub fn check_host_yolo(
    server: &Server,
    key_path: &Path,
    confirm: bool,
) -> Result<(), vk_proto::rpc::RpcError> {
    if !cfg(server).confirm_host_yolo {
        return Ok(());
    }
    let key = workspace_key(key_path);
    if kv_get(server, KV_YOLO, &key).is_some() {
        return Ok(());
    }
    if confirm {
        kv_set(
            server,
            KV_YOLO,
            &key,
            Some(json!({"confirmed_at_ms": vk_store::now_ms()}).to_string()),
        );
        emit(
            server,
            "sandbox.host_yolo_confirmed",
            json!({"workspace": key}),
            json!({}),
        );
        return Ok(());
    }
    Err(err(
        ErrorKind::PermissionDenied,
        format!(
            "confirm_host_yolo: running yolo on the host gives the agent your user account without containment; confirm once for {key} with --confirm-host-yolo (or set isolation.confirm_host_yolo = false)"
        ),
    )
    .details(json!({"reason": "confirm_host_yolo", "workspace": key})))
}

/// `task.create` hook: `--yolo --isolate host` in repo `root`.
pub fn check_task_host_yolo(
    server: &Server,
    req: &IsoRequest,
    root: &Path,
    p: &Value,
) -> Result<(), vk_proto::rpc::RpcError> {
    if !(req.yolo && req.level == IsolationLevel::Host) {
        return Ok(());
    }
    check_host_yolo(server, root, confirm_param(p))
}

pub fn confirm_param(p: &Value) -> bool {
    p.get("confirm_host_yolo")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

// ---- global "allow always" egress approvals (13 §7) -------------------------------------------

/// Persisted global entries (`host` or `host:port`).
pub fn global_entries(server: &Server) -> Vec<String> {
    kv_get(server, KV_GLOBAL, "global")
        .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
        .unwrap_or_default()
}

fn live_proxies(server: &Server) -> Vec<Arc<TaskBox>> {
    server
        .sandbox
        .inner
        .lock()
        .unwrap()
        .boxes
        .values()
        .filter(|b| b.proxy.is_some())
        .cloned()
        .collect()
}

/// Add a global entry: persisted, and pushed into every live proxy.
pub fn add_global(server: &Server, entry: &str) {
    let entry = entry.trim().to_ascii_lowercase();
    if entry.is_empty() {
        return;
    }
    let mut v = global_entries(server);
    if !v.contains(&entry) {
        v.push(entry.clone());
        v.sort();
        kv_set(
            server,
            KV_GLOBAL,
            "global",
            Some(serde_json::to_string(&v).unwrap_or_default()),
        );
    }
    for b in live_proxies(server) {
        if let Some(p) = &b.proxy {
            p.allow_global(&entry);
        }
    }
}

/// Remove a global entry everywhere. Returns whether it existed.
pub fn remove_global(server: &Server, entry: &str) -> bool {
    let entry = entry.trim().to_ascii_lowercase();
    let mut v = global_entries(server);
    let had = v.contains(&entry);
    v.retain(|e| e != &entry);
    kv_set(
        server,
        KV_GLOBAL,
        "global",
        Some(serde_json::to_string(&v).unwrap_or_default()),
    );
    for b in live_proxies(server) {
        if let Some(p) = &b.proxy {
            p.remove_global(&entry);
        }
    }
    had
}

/// Did the answer of an egress Interaction choose "always" (every task) rather than "for this
/// task"? `allow_always` plus `text: "always"|"global"` or the choice `scope = always`.
pub fn answered_global(it: &Interaction) -> bool {
    let Some(a) = &it.answer else { return false };
    if a.decision != Some(Decision::AllowAlways) {
        return false;
    }
    let word = |s: &str| matches!(s.trim().to_ascii_lowercase().as_str(), "always" | "global");
    a.text.as_deref().is_some_and(word)
        || a.choices
            .iter()
            .any(|(q, opts)| q == "scope" && opts.iter().any(|o| word(o)))
}

// ---- manifest-declared needs and the first-use notice (13 §5, §7, §8) ---------------------------

/// Network endpoints, readable and writable home paths the harnesses' manifests declare.
pub fn manifest_needs(
    home: &Path,
    harnesses: &[String],
) -> (Vec<String>, Vec<PathBuf>, Vec<PathBuf>) {
    let (mut allow, mut read, mut write) = (Vec::new(), Vec::new(), Vec::new());
    for h in harnesses {
        let Some((_, l)) = crate::agents::manifests::lookup(h) else {
            continue;
        };
        allow.extend(l.m.sandbox.network.allow.iter().cloned());
        let safe = |p: &String| {
            p.strip_prefix("~/")
                .map(|r| r.trim_end_matches('/'))
                .filter(|r| {
                    !r.is_empty()
                        && Path::new(r)
                            .components()
                            .all(|c| matches!(c, std::path::Component::Normal(_)))
                        && !vk_sandbox::creds::file_refused(Path::new(r))
                })
                .map(|r| home.join(r))
        };
        read.extend(l.m.sandbox.read.iter().filter_map(safe));
        write.extend(l.m.sandbox.write.iter().filter_map(safe));
    }
    (allow, read, write)
}

/// The `[auth]` section of a harness without built-in projection rules.
pub fn declared_auth(harness: &str) -> Option<vk_sandbox::creds::DeclaredAuth> {
    if creds::HarnessAuth::from_id(harness).is_some() {
        return None;
    }
    let (_, l) = crate::agents::manifests::lookup(harness)?;
    let a = &l.m.auth;
    if a.env.is_empty() && a.files.is_empty() {
        return None;
    }
    Some(vk_sandbox::creds::DeclaredAuth {
        env: a.env.clone(),
        files: a.files.clone(),
        home_env: (!a.home_env.is_empty()).then(|| a.home_env.clone()),
    })
}

fn harness_name(h: &str) -> String {
    match h {
        "claude" => "Claude Code".into(),
        "codex" => "Codex".into(),
        "pi" => "pi".into(),
        "omp" => "omp".into(),
        other => crate::agents::manifests::lookup(other)
            .map(|(_, l)| l.m.name.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| other.to_string()),
    }
}

/// What a credential name means to a person ("your ChatGPT auth token").
fn describe_credential(name: &str) -> String {
    match name {
        "env:CLAUDE_CODE_OAUTH_TOKEN" => "your Claude subscription token".into(),
        "env:ANTHROPIC_API_KEY" => "your Anthropic API key".into(),
        "env:OPENAI_API_KEY" => "your OpenAI API key".into(),
        n if n.ends_with(".codex/auth.json") => "your ChatGPT auth token".into(),
        n if n.ends_with(".credentials.json") => "your Claude login".into(),
        n => n
            .strip_prefix("env:")
            .map(|k| format!("${k}"))
            .unwrap_or_else(|| n.trim_start_matches("file:").to_string()),
    }
}

/// Credentials were projected into a new box: record a `credential_use` boundary action and,
/// the first time per harness and level, tell the user what crosses the boundary (13 §8).
pub fn credentials_projected(
    server: &Server,
    key: &str,
    task: Option<&str>,
    level: IsolationLevel,
    network: &str,
    harnesses: &[String],
    names: &[String],
) {
    if names.is_empty() {
        return;
    }
    emit(
        server,
        "sandbox.boundary_action",
        json!({"task": task, "sandbox": key}),
        json!({"kind": "credential_use", "interaction": null, "outcome": "applied", "credentials": names, "harnesses": harnesses}),
    );
    let what: Vec<String> = names.iter().map(|n| describe_credential(n)).collect();
    for h in harnesses {
        let nk = format!("{h}:{}", level.as_str());
        if kv_get(server, KV_NOTICE, &nk).is_some() {
            continue;
        }
        kv_set(
            server,
            KV_NOTICE,
            &nk,
            Some(json!({"at_ms": vk_store::now_ms(), "credentials": names}).to_string()),
        );
        let body = format!(
            "{} in {} will receive {}. Network: {network} profile.",
            harness_name(h),
            level.as_str(),
            what.join(", ")
        );
        server.notify(
            "sandbox",
            None,
            "Credentials projected into a box",
            &body,
            "normal",
        );
        emit(
            server,
            "sandbox.credentials_notice",
            json!({"task": task, "sandbox": key}),
            json!({"harness": h, "level": level.as_str(), "message": body}),
        );
    }
}

// ---- suggest_sandbox_for_yolo and relaunch (13 §3) --------------------------------------------

/// Watch agent events for user-typed host yolo runs (started once, at restore).
pub fn start(server: &Arc<Server>) {
    {
        let mut i = server.sandbox.extras.inner.lock().unwrap();
        if i.listening {
            return;
        }
        i.listening = true;
    }
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let mut rx = server.events.subscribe();
    let weak = Arc::downgrade(server);
    handle.spawn(async move {
        loop {
            let ev = match rx.recv().await {
                Ok(e) => e,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return,
            };
            if !ev.kind.starts_with("agent.") {
                continue;
            }
            let Some(srv) = weak.upgrade() else { return };
            if let Some(run) = ev.subject.get("run").and_then(Value::as_str) {
                maybe_suggest(&srv, run);
            }
        }
    });
}

/// One nudge per run: a live, user-typed yolo run on the host whose harness can resume.
pub fn maybe_suggest(server: &Server, run: &str) {
    let c = cfg(server);
    if !c.suggest_sandbox_for_yolo {
        return;
    }
    let found = server.with_core(|core| {
        let r = core.run(run)?;
        let pane = core.pane(&r.pane)?;
        let ok = r.yolo
            && r.ended_at_ms.is_none()
            && !pane.isolation.is_contained()
            && !r.resume_argv.is_empty()
            && (r.capabilities.is_empty() || r.capabilities.iter().any(|c| c == "resume"));
        ok.then(|| (r.id.clone(), r.pane.clone(), r.harness.clone()))
    });
    let Some((id, pane, harness)) = found else {
        return;
    };
    if !server
        .sandbox
        .extras
        .inner
        .lock()
        .unwrap()
        .suggested
        .insert(id.clone())
    {
        return;
    }
    let level = c.yolo_level();
    emit(
        server,
        "sandbox.suggested",
        json!({"run": id, "pane": pane}),
        json!({"harness": harness, "level": level.as_str(), "hint": format!("vibeke sandbox relaunch {id}")}),
    );
    server.notify(
        "sandbox",
        Some(&pane),
        "Relaunch this yolo run in a sandbox?",
        &format!(
            "{} runs with approvals off on the host. `vibeke sandbox relaunch {id}` restarts it from its session in a {} box.",
            harness_name(&harness),
            level.as_str()
        ),
        "low",
    );
}

/// `sandbox.relaunch {run, isolate?, network?}`: stop a host run and restart its native session
/// inside an isolation level (13 §3 nudge).
async fn relaunch(server: &Arc<Server>, p: &Value) -> R {
    let target = crate::api::req(p, "run")?;
    let run = server
        .with_core(|c| c.run(target).cloned())
        .ok_or_else(|| crate::api::not_found("run", target))?;
    if run.ended_at_ms.is_some() {
        return Err(err(ErrorKind::Conflict, "the run has ended"));
    }
    if run.resume_argv.is_empty() {
        return Err(err(
            ErrorKind::Unsupported,
            "this run has no resume handle; it cannot be relaunched from its session",
        ));
    }
    let contained = server.with_core(|c| {
        c.pane(&run.pane)
            .is_some_and(|p| p.isolation.is_contained())
    });
    if contained {
        return Err(invalid("the run is already contained"));
    }
    let c = cfg(server);
    let level = match s(p, "isolate") {
        Some(l) => IsolationLevel::parse(l)
            .ok_or_else(|| invalid(format!("unknown isolation level {l}")))?,
        None => c.yolo_level(),
    };
    if level == IsolationLevel::Host {
        return Err(invalid(
            "relaunch needs an isolation level (sandbox|container)",
        ));
    }
    let network = match s(p, "network") {
        Some(n) => Some(
            NetworkProfile::parse(n)
                .ok_or_else(|| invalid(format!("unknown network profile {n}")))?,
        ),
        None => None,
    };
    let h =
        crate::agents::Harness::from_id(&run.harness).ok_or_else(|| invalid("unknown harness"))?;
    // Stop the host process (interrupt, then its process group), keep the shell.
    let ctx = crate::drafts::user_ctx();
    let _ = Box::pin(crate::api::dispatch(
        server,
        &ctx,
        "agent.interrupt",
        &json!({"target": run.id}),
    ))
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    if let Some(st) = server
        .pane_rt(&run.pane)
        .and_then(|rt| rt.status.lock().unwrap().clone())
        && let Some(g) = st.fg_pgid.filter(|g| *g != st.child_pid && *g > 1)
    {
        // SAFETY: signalling a process group of a pane this server owns.
        unsafe { libc::killpg(g as i32, libc::SIGTERM) };
    }
    server.agents.end_run(server, &run.id, "relaunched");
    // Wait for the pane's shell to be back in the foreground.
    for _ in 0..50 {
        let back = server
            .pane_rt(&run.pane)
            .and_then(|rt| rt.status.lock().unwrap().clone())
            .is_none_or(|st| st.fg_pgid.is_none_or(|g| g == st.child_pid));
        if back {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let opts = LaunchOpts {
        yolo: run.yolo,
        isolate: Some(level),
        network,
        confirm_host_yolo: false,
    };
    let launch = prepare_agent(server, &run.pane, h.id(), run.resume_argv.clone(), &opts).await?;
    let new = {
        let mut core = server.core.lock().unwrap();
        let mut r = crate::agents::new_run(
            &mut core,
            &run.pane,
            h,
            "process",
            StateSource::Process,
            0.6,
        );
        r.name = run.name.clone();
        r.harness_session_id = run.harness_session_id.clone();
        r.resume_argv = run.resume_argv.clone();
        r.task = run.task.clone();
        r.yolo = run.yolo;
        let mut tx = Tx::new();
        tx.counters = true;
        tx.event(
            "agent.started",
            json!({"run": r.id, "pane": run.pane}),
            json!({"harness": h.id(), "via": "relaunch", "resumed_from": run.id, "isolation": level.as_str()}),
        );
        tx.run(r.clone());
        server.commit(&mut core, tx).map_err(internal)?;
        r
    };
    crate::render::write_and_ack(
        server,
        &run.pane,
        server.next_internal_input_id(),
        format!("{}\r", launch.line).into_bytes(),
    )
    .await;
    emit(
        server,
        "sandbox.relaunched",
        json!({"run": new.id, "pane": run.pane}),
        json!({"from": run.id, "level": level.as_str()}),
    );
    Ok(json!({"run": new, "from": run.id, "level": level.as_str()}))
}

// ---- crash handling, idle suspend, resource pressure (13 §11) ---------------------------------

fn container_task_boxes(server: &Server) -> Vec<Arc<TaskBox>> {
    server
        .sandbox
        .inner
        .lock()
        .unwrap()
        .boxes
        .values()
        .filter(|b| b.task.is_some() && matches!(b.runner, BoxRunner::Container(_)))
        .cloned()
        .collect()
}

/// Vibeke stops/removes `key` on purpose (sandbox stop/remove, park, finish): not a crash.
/// Call it before the stop (so a state read during the stop is not taken for a crash) and
/// again once the stop returned: a `Running` read that began before that second call (one
/// racing the stop) then cannot cancel the expectation.
pub fn expect_down(server: &Server, key: &str) {
    let mut i = server.sandbox.extras.inner.lock().unwrap();
    i.expected_down.insert(key.to_string(), Instant::now());
    i.paused.remove(key);
}

/// Housekeeping, called from the sandbox tick (every 2 s while boxes exist); the expensive
/// parts throttle themselves and run off the executor.
pub fn tick(server: &Arc<Server>) {
    let c = cfg(server);
    let now = Instant::now();
    let (state_due, stats_due, ports_due) = {
        let mut i = server.sandbox.extras.inner.lock().unwrap();
        if i.polling {
            return;
        }
        let due =
            |t: Option<Instant>, every: Duration| t.is_none_or(|t| now.duration_since(t) >= every);
        let s = due(i.last_state_poll, STATE_POLL);
        let st = due(i.last_stats_poll, c.resource_poll_every());
        let pt = c.container.discover_ports && due(i.last_ports_poll, PORTS_POLL);
        if !(s || st || pt) {
            return;
        }
        if s {
            i.last_state_poll = Some(now);
        }
        if st {
            i.last_stats_poll = Some(now);
        }
        if pt {
            i.last_ports_poll = Some(now);
        }
        i.polling = true;
        (s, st, pt)
    };
    let boxes = container_task_boxes(server);
    let srv = server.clone();
    tokio::spawn(async move {
        if state_due {
            poll_states(&srv, &boxes, &c).await;
        }
        if stats_due {
            poll_stats(&srv, &boxes, &c).await;
        }
        if ports_due {
            super::forward::poll_ports(&srv, &boxes).await;
        }
        srv.sandbox.extras.inner.lock().unwrap().polling = false;
    });
}

async fn poll_states(server: &Arc<Server>, boxes: &[Arc<TaskBox>], c: &IsolationConfig) {
    let bs: Vec<Arc<TaskBox>> = boxes.to_vec();
    let states = tokio::task::spawn_blocking(move || {
        bs.iter()
            .map(|b| {
                let at = Instant::now();
                let st = match &b.runner {
                    BoxRunner::Container(c) => c.b().state(),
                    _ => BoxState::Other,
                };
                (b.clone(), st, at)
            })
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    let idle_after = c.idle_suspend_after();
    for (b, st, at) in states {
        observe_state_at(server, &b, st, idle_after, at).await;
    }
}

/// Feed one observed state (also the test hook).
pub async fn observe_state(
    server: &Arc<Server>,
    b: &Arc<TaskBox>,
    st: BoxState,
    idle_after: Option<Duration>,
) {
    observe_state_at(server, b, st, idle_after, Instant::now()).await;
}

/// Feed one state whose read began at `read_at`.
async fn observe_state_at(
    server: &Arc<Server>,
    b: &Arc<TaskBox>,
    st: BoxState,
    idle_after: Option<Duration>,
    read_at: Instant,
) {
    let key = b.key.clone();
    match st {
        BoxState::Running => {
            let idle = box_idle(server, b, idle_after);
            let suspend = {
                let mut i = server.sandbox.extras.inner.lock().unwrap();
                // Running again after Vibeke stopped it: a later disappearance is a crash. A read
                // that began before the stop was decided says nothing about after it.
                if i.expected_down.get(&key).is_some_and(|t| *t <= read_at) {
                    i.expected_down.remove(&key);
                }
                i.paused.remove(&key);
                let w = i.watch.entry(key.clone()).or_default();
                w.seen_running = true;
                w.last_running_ms = vk_store::now_ms();
                match (idle, idle_after) {
                    (true, Some(after)) => {
                        let since = *w.idle_since.get_or_insert_with(Instant::now);
                        since.elapsed() >= after
                    }
                    _ => {
                        w.idle_since = None;
                        false
                    }
                }
            };
            if suspend {
                idle_suspend(server, b).await;
            }
        }
        BoxState::Paused | BoxState::Other => {}
        BoxState::Missing | BoxState::Stopped => {
            let lost = {
                let mut i = server.sandbox.extras.inner.lock().unwrap();
                let expected = i.expected_down.contains_key(&key);
                let w = i.watch.entry(key.clone()).or_default();
                let was = w.seen_running;
                w.seen_running = false;
                w.idle_since = None;
                (was && !expected).then_some(w.last_running_ms)
            };
            if let Some(since) = lost {
                runner_lost(server, b, since, st);
            }
        }
    }
}

/// No working run, no open interaction and no input in the box's panes for the idle period.
fn box_idle(server: &Server, b: &TaskBox, idle_after: Option<Duration>) -> bool {
    let Some(after) = idle_after else {
        return false;
    };
    let Some(task) = b.task.as_deref() else {
        return false;
    };
    let panes: Vec<String> = server.with_core(|c| {
        let ws = c.task(task).and_then(|t| t.workspace.clone());
        c.model
            .panes
            .iter()
            .filter(|p| ws.as_deref() == Some(p.workspace.as_str()))
            .map(|p| p.id.clone())
            .collect()
    });
    let busy = server.with_core(|c| {
        c.model.runs.iter().any(|r| {
            r.ended_at_ms.is_none()
                && panes.contains(&r.pane)
                && r.execution.value == Execution::Working
        }) || c
            .model
            .interactions
            .iter()
            .any(|i| i.status == InteractionStatus::Open && panes.contains(&i.pane))
    });
    if busy {
        return false;
    }
    !panes.iter().any(|p| {
        server
            .pane_rt(p)
            .and_then(|rt| *rt.last_input.lock().unwrap())
            .is_some_and(|t| t.elapsed() < after)
    })
}

async fn idle_suspend(server: &Arc<Server>, b: &Arc<TaskBox>) {
    let BoxRunner::Container(_) = &b.runner else {
        return;
    };
    let bb = b.clone();
    let r = tokio::task::spawn_blocking(move || match &bb.runner {
        BoxRunner::Container(c) => c.b().pause(),
        _ => Ok(()),
    })
    .await;
    match r {
        Ok(Ok(())) => {
            {
                let mut i = server.sandbox.extras.inner.lock().unwrap();
                i.paused.insert(b.key.clone());
                if let Some(w) = i.watch.get_mut(&b.key) {
                    w.idle_since = None;
                }
            }
            emit(
                server,
                "sandbox.suspended",
                json!({"task": b.task, "sandbox": b.key}),
                json!({"reason": "idle", "mode": "pause"}),
            );
        }
        Ok(Err(e)) => tracing::info!(sandbox = %b.key, error = %e, "idle suspend skipped"),
        Err(_) => {}
    }
}

/// Is `key` paused by idle suspend?
pub fn is_paused(server: &Server, key: &str) -> bool {
    server
        .sandbox
        .extras
        .inner
        .lock()
        .unwrap()
        .paused
        .contains(key)
}

/// Thaw an idle-suspended box (blocking CLI call when it is paused). `reason` goes into the
/// `sandbox.resumed` event.
pub fn wake_blocking(server: &Server, b: &TaskBox, reason: &str) {
    if !server
        .sandbox
        .extras
        .inner
        .lock()
        .unwrap()
        .paused
        .remove(&b.key)
    {
        return;
    }
    if let BoxRunner::Container(c) = &b.runner
        && let Err(e) = c.b().unpause()
    {
        tracing::warn!(sandbox = %b.key, error = %e, "unpause failed");
    }
    if let Some(w) = server
        .sandbox
        .extras
        .inner
        .lock()
        .unwrap()
        .watch
        .get_mut(&b.key)
    {
        w.idle_since = None;
    }
    emit(
        server,
        "sandbox.resumed",
        json!({"task": b.task, "sandbox": b.key}),
        json!({"reason": reason, "mode": "unpause"}),
    );
}

/// A new pane is about to `exec` into box `b`: a paused box must run first.
pub fn before_spawn(server: &Server, b: &TaskBox) {
    if is_paused(server, &b.key) {
        wake_blocking(server, b, "spawn");
    }
}

/// Input for `pane` (render path): thaw its box when it is paused. Cheap when nothing is.
pub fn touch_pane(server: &Arc<Server>, pane: &str) {
    if server
        .sandbox
        .extras
        .inner
        .lock()
        .unwrap()
        .paused
        .is_empty()
    {
        return;
    }
    let key = server.with_core(|c| {
        let p = c.pane(pane)?;
        (p.isolation.level == IsolationLevel::Container)
            .then(|| c.ws(&p.workspace).and_then(|w| w.task.clone()))
            .flatten()
    });
    let Some(b) = key.and_then(|k| server.sandbox.get(&k)) else {
        return;
    };
    if !is_paused(server, &b.key) {
        return;
    }
    let srv = server.clone();
    tokio::task::spawn_blocking(move || wake_blocking(&srv, &b, "input"));
}

/// The box died under its task (13 §11): end its runs with `runner_lost`, cancel their
/// interactions, remember the resumable ones for `sandbox.recover`.
fn runner_lost(server: &Arc<Server>, b: &TaskBox, since_ms: i64, st: BoxState) {
    let Some(task) = b.task.clone() else { return };
    let (panes, live, recent): (Vec<String>, Vec<AgentRun>, Vec<AgentRun>) =
        server.with_core(|c| {
            let ws = c.task(&task).and_then(|t| t.workspace.clone());
            let panes: Vec<String> = c
                .model
                .panes
                .iter()
                .filter(|p| ws.as_deref() == Some(p.workspace.as_str()))
                .map(|p| p.id.clone())
                .collect();
            let in_task =
                |r: &&AgentRun| panes.contains(&r.pane) || r.task.as_deref() == Some(task.as_str());
            let live = c
                .model
                .runs
                .iter()
                .filter(in_task)
                .filter(|r| r.ended_at_ms.is_none())
                .cloned()
                .collect();
            // Runs whose process ended with the box (their exec died first).
            let recent = c
                .model
                .runs
                .iter()
                .filter(in_task)
                .filter(|r| {
                    r.ended_at_ms.is_some_and(|t| t >= since_ms - 5_000)
                        && r.execution.detail.as_deref() == Some("exited")
                })
                .cloned()
                .collect();
            (panes, live, recent)
        });
    // Open interactions on the task's panes first (egress, boundary requests and the runs'
    // own): closing them releases any gate held for them (04 §2.5 rule 1).
    let open: Vec<String> = server.with_core(|c| {
        c.model
            .interactions
            .iter()
            .filter(|i| i.status == InteractionStatus::Open && panes.contains(&i.pane))
            .map(|i| i.id.clone())
            .collect()
    });
    for id in open {
        crate::agents::close_interaction(server, &id, InteractionStatus::Cancelled, "runner_lost");
    }
    for r in &live {
        server.agents.end_run(server, &r.id, "runner_lost");
    }
    if !recent.is_empty() {
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        for r in &recent {
            let mut r = r.clone();
            r.execution.detail = Some("runner_lost".into());
            tx.event(
                "agent.exited",
                json!({"run": r.id, "pane": r.pane}),
                json!({"reason": "runner_lost", "harness": r.harness}),
            );
            tx.run(r);
        }
        let _ = server.commit(&mut c, tx);
    }
    let lost: Vec<&AgentRun> = live.iter().chain(recent.iter()).collect();
    let resumable: Vec<String> = lost
        .iter()
        .filter(|r| !r.resume_argv.is_empty() || crate::agents::headless::is_headless(r))
        .map(|r| r.id.clone())
        .collect();
    kv_set(
        server,
        KV_LOST,
        &b.key,
        Some(json!({"runs": resumable, "at_ms": vk_store::now_ms()}).to_string()),
    );
    server
        .sandbox
        .extras
        .inner
        .lock()
        .unwrap()
        .expected_down
        .insert(b.key.clone(), Instant::now());
    emit(
        server,
        "sandbox.runner_lost",
        json!({"task": task, "sandbox": b.key}),
        json!({"state": st.as_str(), "runs": lost.iter().map(|r| &r.id).collect::<Vec<_>>(), "resumable": resumable}),
    );
    let pane = panes.first().cloned();
    server.notify(
        "sandbox",
        pane.as_deref(),
        "Task box lost",
        &format!(
            "The container box of this task is {}. {} run(s) ended; `vibeke sandbox recover {task}` starts a fresh box and resumes them.",
            st.as_str(),
            lost.len()
        ),
        "high",
    );
}

async fn poll_stats(server: &Arc<Server>, boxes: &[Arc<TaskBox>], c: &IsolationConfig) {
    let bs: Vec<Arc<TaskBox>> = boxes.to_vec();
    let samples = tokio::task::spawn_blocking(move || {
        bs.iter()
            .filter_map(|b| match &b.runner {
                BoxRunner::Container(c) if c.b().state() == BoxState::Running => {
                    c.b().stats().map(|s| (b.clone(), s))
                }
                _ => None,
            })
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    for (b, s) in samples {
        observe_stats(server, &b, &s, c.resource_pressure);
    }
}

/// Feed one resource sample (also the test hook).
pub fn observe_stats(server: &Server, b: &TaskBox, s: &boxops::BoxStats, threshold: f64) {
    let BoxRunner::Container(c) = &b.runner else {
        return;
    };
    let limits = &c.b().spec.limits;
    let usage = json!({
        "cpu_percent": s.cpu_percent, "memory_bytes": s.mem_bytes, "memory_limit_bytes": s.mem_limit,
        "pids": s.pids,
        "limits": {"cpus": limits.cpus, "memory": limits.memory, "pids": limits.pids.unwrap_or(1024)},
        "sampled_at_ms": vk_store::now_ms(),
    });
    server
        .sandbox
        .extras
        .inner
        .lock()
        .unwrap()
        .usage
        .insert(b.key.clone(), usage);
    for p in boxops::pressure(s, limits, threshold) {
        let send = {
            let mut i = server.sandbox.extras.inner.lock().unwrap();
            let k = (b.key.clone(), p.resource);
            let fresh = i
                .pressure_sent
                .get(&k)
                .is_some_and(|t| t.elapsed() < PRESSURE_EVERY);
            if !fresh {
                i.pressure_sent.insert(k, Instant::now());
            }
            !fresh
        };
        if !send {
            continue;
        }
        emit(
            server,
            "sandbox.resource_pressure",
            json!({"task": b.task, "sandbox": b.key}),
            json!({"resource": p.resource, "value": p.value, "limit": p.limit, "share": p.share}),
        );
        server.notify(
            "sandbox",
            None,
            "Box under resource pressure",
            &format!(
                "{} is at {:.0}% of its {} limit",
                b.task.as_deref().unwrap_or(&b.key),
                p.share * 100.0,
                p.resource
            ),
            "normal",
        );
    }
}

/// Last usage sample and limits of a box (for `sandbox.list`).
pub fn usage(server: &Server, key: &str) -> Option<Value> {
    server
        .sandbox
        .extras
        .inner
        .lock()
        .unwrap()
        .usage
        .get(key)
        .cloned()
}

/// `sandbox.recover {task}`: a fresh box for a task whose box was lost — pull what the old clone
/// holds, re-create the box (template, private clone, lifecycle) and resume the lost runs.
async fn recover(server: &Arc<Server>, p: &Value) -> R {
    let t = crate::api::req(p, "task")?;
    let task = server
        .with_core(|c| c.task(t).cloned())
        .ok_or_else(|| crate::api::not_found("task", t))?;
    let tb = server
        .sandbox
        .get(&task.id)
        .ok_or_else(|| crate::api::not_found("sandbox", t))?;
    if !matches!(tb.runner, BoxRunner::Container(_)) {
        return Err(invalid("not a container box"));
    }
    let srv = server.clone();
    let tb2 = tb.clone();
    let (created, synced) = tokio::task::spawn_blocking(move || {
        let BoxRunner::Container(c) = &tb2.runner else {
            unreachable!()
        };
        // The old box's repo persists host-side: bring its commits to the host first.
        let synced = match c.clone.as_ref() {
            Some(cl) if cl.dir.join(".git").exists() => container::sync_task(c, "pull", false)
                .map(|o| {
                    container::record_sync(&srv, &tb2, &o);
                    serde_json::to_value(&o).unwrap_or_default()
                })
                .unwrap_or_else(|e| json!({"error": e.message})),
            _ => Value::Null,
        };
        let created = container::ensure(&srv, &tb2.key, tb2.task.as_deref(), c)?;
        Ok::<_, vk_proto::rpc::RpcError>((created, synced))
    })
    .await
    .map_err(internal)??;
    {
        let mut i = server.sandbox.extras.inner.lock().unwrap();
        i.expected_down.remove(&tb.key);
        i.paused.remove(&tb.key);
    }
    // Re-bind brokers of live container panes (their old in-box sockets went with the box).
    if let Some(l) = link(server, &tb.key) {
        let panes: Vec<String> = server.with_core(|c| {
            c.model
                .panes
                .iter()
                .filter(|p| task.workspace.as_deref() == Some(p.workspace.as_str()))
                .map(|p| p.id.clone())
                .collect()
        });
        for pn in panes {
            l.add_pane(&pn);
        }
    }
    let ids: Vec<String> = kv_get(server, KV_LOST, &tb.key)
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v["runs"].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    let mut resumed = vec![];
    let mut skipped = vec![];
    for id in ids {
        let Some(run) = server.with_core(|c| {
            c.run(&id)
                .cloned()
                .or_else(|| c.store.find::<AgentRun>("run", &id).ok().flatten())
        }) else {
            skipped.push(json!({"run": id, "reason": "run record not found"}));
            continue;
        };
        let pane = match &task.workspace {
            Some(ws) => {
                let cwd = task.worktree_path.clone().or(run.cwd.clone());
                match server.create_tab(ws, cwd.as_deref(), None, None, None) {
                    Ok((_, pn)) => {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        Some(pn.id)
                    }
                    Err(e) => {
                        skipped.push(json!({"run": id, "reason": e.to_string()}));
                        continue;
                    }
                }
            }
            None => None,
        };
        match crate::agents::resume_from(server, run, pane).await {
            Ok(v) => resumed.push(v["run"].clone()),
            Err(e) => skipped.push(json!({"run": id, "reason": e.message})),
        }
    }
    kv_set(server, KV_LOST, &tb.key, None);
    emit(
        server,
        "sandbox.recovered",
        json!({"task": task.id, "sandbox": tb.key}),
        json!({"created": created, "resumed": resumed.len(), "skipped": skipped.len()}),
    );
    Ok(
        json!({"task": task.id, "created": created, "synced": synced, "resumed": resumed, "skipped": skipped}),
    )
}

// ---- task park / resume hooks (13 §11) ----------------------------------------------------------

/// `task.park` stopped the task's agents: stop its container box too (park = stop).
pub async fn on_task_parked(server: &Arc<Server>, task: &str) {
    let Some(b) = server.sandbox.get(task) else {
        return;
    };
    if !matches!(b.runner, BoxRunner::Container(_)) {
        return;
    }
    expect_down(server, &b.key);
    let bb = b.clone();
    let r = tokio::task::spawn_blocking(move || match &bb.runner {
        BoxRunner::Container(c) => c.b().stop(),
        _ => Ok(()),
    })
    .await;
    if matches!(r, Ok(Ok(()))) {
        expect_down(server, &b.key);
        emit(
            server,
            "sandbox.suspended",
            json!({"task": b.task, "sandbox": b.key}),
            json!({"reason": "park", "mode": "stop"}),
        );
    }
}

/// `task.resume`: the box must run before the agents restart in it.
pub async fn on_task_resumed(server: &Arc<Server>, task: &str) {
    let Some(b) = server.sandbox.get(task) else {
        return;
    };
    if !matches!(b.runner, BoxRunner::Container(_)) {
        return;
    }
    let (srv, bb) = (server.clone(), b.clone());
    let r = tokio::task::spawn_blocking(move || match &bb.runner {
        BoxRunner::Container(c) => {
            srv.sandbox
                .extras
                .inner
                .lock()
                .unwrap()
                .paused
                .remove(&bb.key);
            container::ensure(&srv, &bb.key, bb.task.as_deref(), c)
        }
        _ => Ok(false),
    })
    .await;
    if matches!(r, Ok(Ok(_))) {
        emit(
            server,
            "sandbox.resumed",
            json!({"task": b.task, "sandbox": b.key}),
            json!({"reason": "task_resume"}),
        );
    }
}

// ---- shell, logs, prune, setup-token (13 §11, §8) ----------------------------------------------

fn box_of(server: &Server, p: &Value) -> Result<Arc<TaskBox>, vk_proto::rpc::RpcError> {
    let t = crate::api::req(p, "task")?;
    let key = server
        .with_core(|c| c.task(t).map(|x| x.id.clone()))
        .unwrap_or_else(|| t.to_string());
    server
        .sandbox
        .get(&key)
        .ok_or_else(|| crate::api::not_found("sandbox", t))
}

/// `sandbox.shell {task}`: the argv of an interactive debugging shell in the task's box (run by
/// the CLI in the user's terminal). Container: `<runtime> exec -it <box> <shell> -l`. Sandbox:
/// the Seatbelt/bubblewrap wrapper around a login shell, with the box's env (no credentials).
async fn shell(server: &Arc<Server>, p: &Value) -> R {
    let b = box_of(server, p)?;
    let c = cfg(server);
    let term = server
        .opts
        .env
        .iter()
        .find(|(k, _)| k == "TERM")
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| "xterm-256color".into());
    match &b.runner {
        BoxRunner::Container(cb) => {
            let bb = b.clone();
            let srv = server.clone();
            tokio::task::spawn_blocking(move || {
                wake_blocking(&srv, &bb, "shell");
                Ok::<_, vk_proto::rpc::RpcError>(())
            })
            .await
            .map_err(internal)??;
            let argv = cb.b().spec.shell_argv(
                &c.container.shell,
                &[
                    ("TERM".into(), term),
                    ("VIBEKE_SANDBOX_SHELL".into(), "1".into()),
                ],
            );
            Ok(
                json!({"sandbox": b.key, "level": "container", "argv": argv, "env": cb.b().cli_env.iter().map(|(k, v)| json!([k, v])).collect::<Vec<_>>(), "cwd": Value::Null}),
            )
        }
        BoxRunner::Vm(v) => {
            let argv = v.backend.exec_argv(
                &v.vm,
                &[v.shell.clone(), "-l".into()],
                Some(vk_sandbox::vm::VM_WORKSPACE),
                &[
                    ("TERM".into(), term),
                    ("VIBEKE_SANDBOX_SHELL".into(), "1".into()),
                ],
            );
            Ok(
                json!({"sandbox": b.key, "level": "vm", "argv": argv, "env": [], "cwd": Value::Null}),
            )
        }
        BoxRunner::Sandbox(r) => {
            let shell = server
                .opts
                .env
                .iter()
                .find(|(k, _)| k == "SHELL")
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| "/bin/sh".into());
            let pane = format!("shell-{}", &ulid()[20..]);
            let prepared = r
                .prepare(SpawnRequest {
                    pane_id: pane,
                    argv: vec![shell, "-l".into()],
                    cwd: b.checkout.clone(),
                    env: vec![("TERM".into(), term)],
                })
                .map_err(|e| err(ErrorKind::Unsupported, e.to_string()))?;
            // Credentials are not part of a debugging shell.
            let secret_names: Vec<String> = b
                .projection_names
                .iter()
                .filter_map(|n| n.strip_prefix("env:").map(str::to_string))
                .collect();
            let env: Vec<Value> = prepared
                .env
                .iter()
                .filter(|(k, _)| !secret_names.contains(k))
                .map(|(k, v)| json!([k, v]))
                .collect();
            Ok(
                json!({"sandbox": b.key, "level": "sandbox", "argv": prepared.argv, "env": env, "cwd": prepared.cwd}),
            )
        }
    }
}

/// Recent `sandbox.*` / `task.synced` events of one box.
fn box_events(server: &Server, key: &str, task: Option<&str>, limit: usize) -> Vec<Value> {
    let types = ["sandbox.*".to_string(), "task.synced".to_string()];
    let evs = server.with_core(|c| {
        let last = c.store.last_seq().unwrap_or(0);
        c.store
            .events_after((last - 50_000).max(0), 50_000, &types)
            .unwrap_or_default()
    });
    let mut v: Vec<Value> = evs
        .into_iter()
        .filter(|e| e.kind.starts_with("sandbox.") || e.kind == "task.synced")
        .filter(|e| {
            e.subject.get("sandbox").and_then(Value::as_str) == Some(key)
                || (task.is_some() && e.subject.get("task").and_then(Value::as_str) == task)
        })
        .map(|e| json!({"seq": e.seq, "ts": e.ts, "type": e.kind, "data": e.data}))
        .collect();
    let n = v.len();
    if n > limit {
        v.drain(..n - limit);
    }
    v
}

/// `sandbox.logs {task, tail?}`: the box's own output, its setup log and its recent events.
async fn logs(server: &Arc<Server>, p: &Value) -> R {
    let b = box_of(server, p)?;
    let tail = p
        .get("tail")
        .and_then(Value::as_u64)
        .unwrap_or(200)
        .clamp(1, 5000) as u32;
    let root = sbx_root(&b.key);
    let setup = std::fs::read_to_string(root.join("setup.log")).ok();
    let bb = b.clone();
    let container_log = tokio::task::spawn_blocking(move || match &bb.runner {
        BoxRunner::Container(c) => Some(c.b().logs(tail)),
        _ => None,
    })
    .await
    .unwrap_or(None);
    let events = box_events(server, &b.key, b.task.as_deref(), tail as usize);
    Ok(json!({
        "sandbox": b.key, "task": b.task, "level": b.isolation.level.as_str(),
        "container": container_log, "setup": setup, "events": events,
    }))
}

/// `sandbox.prune {dry_run?}`: remove what no task owns any more — boxes of this session whose
/// key has no context (and is not a warm-pool box), `<state>/sbx/*` dirs without a record,
/// and stale `/tmp` broker dirs. Kept boxes with unsynced work are contexts, so never pruned.
async fn prune(server: &Arc<Server>, p: &Value) -> R {
    let dry = p.get("dry_run").and_then(Value::as_bool).unwrap_or(false);
    let c = cfg(server);
    let known: HashSet<String> = server
        .sandbox
        .inner
        .lock()
        .unwrap()
        .boxes
        .keys()
        .cloned()
        .collect();
    let names: HashSet<String> = known
        .iter()
        .map(|k| format!("vk-{}", vk_sandbox::runner::short_id(k)))
        .chain(super::pool::claimed_names(server))
        .collect();
    let pooled = super::pool::warm_names(server);
    // Contained tasks keep their record (and box dir) even when their context is not loaded.
    let kv_keys: HashSet<String> = server.with_core(|core| {
        core.model
            .tasks
            .iter()
            .filter(|t| t.isolation.is_contained())
            .map(|t| t.id.clone())
            .collect()
    });
    let session = server.opts.session.clone();
    let runtime = server
        .sandbox
        .container_runtime()
        .map(|p| p.to_string_lossy().into_owned())
        .or(c.container.runtime.clone());
    let env = server.opts.env.clone();
    let state = paths::state_root().join("sbx");
    let live_ids: HashSet<String> = known
        .iter()
        .chain(kv_keys.iter())
        .map(|k| vk_sandbox::runner::short_id(k))
        .chain(super::pool::slot_ids(server))
        .collect();
    let r = tokio::task::spawn_blocking(move || {
        let mut boxes = vec![];
        let mut warnings = vec![];
        let provider = match runtime.as_deref() {
            Some(r) => vk_sandbox::container::Provider::from_config(r),
            None => vk_sandbox::container::detect(),
        };
        if let Some(prov) = provider {
            let cli_env = vk_sandbox::container::cli_env(&env);
            match boxops::list_boxes(&prov, &cli_env) {
                Ok(list) => {
                    for lb in list {
                        if lb.session != session || names.contains(&lb.name) || pooled.contains(&lb.name) {
                            continue;
                        }
                        let mut removed = false;
                        if !dry {
                            let argv = vec![
                                prov.cli().to_string_lossy().into_owned(),
                                "rm".into(),
                                "--force".into(),
                                lb.name.clone(),
                            ];
                            removed = vk_sandbox::container::run_cmd(&argv, &cli_env, None, Duration::from_secs(60))
                                .is_ok_and(|o| o.ok);
                        }
                        boxes.push(json!({"container": lb.name, "key": lb.key, "state": lb.state, "removed": removed}));
                    }
                }
                Err(e) => warnings.push(format!("listing boxes failed: {e}")),
            }
        }
        let mut dirs = vec![];
        for e in std::fs::read_dir(&state).into_iter().flatten().flatten() {
            let id = e.file_name().to_string_lossy().into_owned();
            if live_ids.contains(&id) || !e.path().is_dir() {
                continue;
            }
            let removed = !dry && std::fs::remove_dir_all(e.path()).is_ok();
            dirs.push(json!({"dir": e.path(), "removed": removed}));
        }
        let tmp = PathBuf::from(format!("/tmp/vibeke-{}-bx", unsafe { libc::getuid() }));
        for e in std::fs::read_dir(&tmp).into_iter().flatten().flatten() {
            let id = e.file_name().to_string_lossy().into_owned();
            if live_ids.contains(&id) {
                continue;
            }
            let removed = !dry && std::fs::remove_dir_all(e.path()).is_ok();
            dirs.push(json!({"dir": e.path(), "removed": removed}));
        }
        json!({"dry_run": dry, "containers": boxes, "dirs": dirs, "warnings": warnings})
    })
    .await
    .map_err(internal)?;
    if !dry {
        emit(
            server,
            "sandbox.pruned",
            json!({}),
            json!({"containers": r["containers"].as_array().map(|a| a.len()).unwrap_or(0), "dirs": r["dirs"].as_array().map(|a| a.len()).unwrap_or(0)}),
        );
    }
    Ok(r)
}

/// `sandbox.setup_token {token}`: store the output of `claude setup-token` for projection into
/// boxes as `CLAUDE_CODE_OAUTH_TOKEN` (13 §8). The value never appears in events or results.
fn setup_token(p: &Value) -> R {
    let token = crate::api::req(p, "token")?;
    let path = creds::store_claude_setup_token(&credentials_dir(), token)
        .map_err(|e| invalid(e.to_string()))?;
    Ok(json!({"stored": true, "path": path, "env": "CLAUDE_CODE_OAUTH_TOKEN"}))
}

/// `sandbox.allow {global: true}` / `sandbox.disallow`.
fn disallow(server: &Server, p: &Value) -> R {
    let host = crate::api::req(p, "host")?.to_ascii_lowercase();
    if p.get("global").and_then(Value::as_bool).unwrap_or(false) || s(p, "task").is_none() {
        let removed = remove_global(server, &host);
        emit(
            server,
            "sandbox.egress_revoked",
            json!({}),
            json!({"host": host, "scope": "global"}),
        );
        return Ok(json!({"host": host, "scope": "global", "removed": removed}));
    }
    let b = box_of(server, p)?;
    let pol = b
        .proxy
        .as_ref()
        .map(|p| p.policy.clone())
        .ok_or_else(|| invalid("this sandbox has no egress proxy"))?;
    let removed = pol
        .write()
        .map(|mut w| w.task_allow.remove(&host))
        .unwrap_or(false);
    emit(
        server,
        "sandbox.egress_revoked",
        json!({"task": b.task, "sandbox": b.key}),
        json!({"host": host, "scope": "task"}),
    );
    Ok(json!({"host": host, "scope": "task", "task": b.task, "removed": removed}))
}

/// Methods served here (dispatched from `sandbox::api`).
pub async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    let user_only = matches!(
        method,
        "sandbox.shell"
            | "sandbox.logs"
            | "sandbox.prune"
            | "sandbox.recover"
            | "sandbox.relaunch"
            | "sandbox.setup_token"
            | "sandbox.disallow"
    );
    if user_only && ctx.pane_scope.is_some() {
        return Some(Err(err(
            ErrorKind::PermissionDenied,
            format!("{method} needs a user client"),
        )));
    }
    Some(match method {
        "sandbox.shell" => shell(server, p).await,
        "sandbox.logs" => logs(server, p).await,
        "sandbox.prune" => prune(server, p).await,
        "sandbox.recover" => recover(server, p).await,
        "sandbox.relaunch" => relaunch(server, p).await,
        "sandbox.setup_token" => setup_token(p),
        "sandbox.disallow" => disallow(server, p),
        _ => return None,
    })
}
