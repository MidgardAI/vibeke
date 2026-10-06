//! Orchestration above single tasks (spec 12, Batch 4 of the gap audit): the API hub.
//!
//! Everything here sits behind `[orchestrate.*] enabled` flags that default **off** (a call to
//! a disabled feature fails with `unsupported` and says which flag to set). The logic lives in
//! `vk-orchestrate` and `vk-sandbox::vm`; the modules below connect it to tasks, runs, the
//! store and the event log:
//!
//! | Module | Methods |
//! |---|---|
//! | `orch_family` | best-of-N: `task.best_of_n`, `task.compare`, `task.pick`, `family.*` (05 §12) |
//! | `orch_split` | `task.split` (05 §11) |
//! | `orch_learn` | `policy.learned.*` (04 §7.7, 12) |
//! | `orch_merge` | claims, `merge.predict`, `merge.queue.*` (12, 05 §10) |
//! | `orch_goal` | `goal.*` planner, approval gate, briefing (12) |
//! | `orch_quota` | `quota.*` scheduling (12) |
//! | `orch_vm` | `vm.*` and the `vm` isolation level (13 §2.1, §9) |
//!
//! State lives in store entities (`orch_family`, `orch_goal`, `orch_claim`, `orch_queue`) and
//! two kv scopes (`orch.counters`, `orch.learned`, `orch.quota`), so it survives restarts and
//! needs no model changes. Mutating methods are full scope only; claims are the one thing a
//! pane may do (for its own task).

use crate::Server;
use crate::api::{Ctx, R, err, internal, invalid};
use crate::core::{Core, Tx};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use vk_orchestrate::OrchestrateConfig;
use vk_proto::rpc::ErrorKind;

pub const METHODS: &[(&str, bool)] = &[
    // best-of-N (05 §12)
    ("family.list", false),
    ("family.get", false),
    ("family.check", true),
    ("task.best_of_n", true),
    ("task.compare", false),
    ("task.pick", true),
    // split into task (05 §11)
    ("task.split", true),
    // learned policy
    ("policy.learned.list", false),
    ("policy.learned.accept", true),
    ("policy.learned.dismiss", true),
    // merge orchestration
    ("task.claim", true),
    ("task.claim.list", false),
    ("task.claim.remove", true),
    ("merge.predict", false),
    ("merge.queue.add", true),
    ("merge.queue.list", false),
    ("merge.queue.cancel", true),
    ("merge.queue.requeue", true),
    ("merge.queue.run", true),
    // goal planner
    ("goal.create", true),
    ("goal.list", false),
    ("goal.get", false),
    ("goal.plan", true),
    ("goal.plan_submit", true),
    ("goal.approve", true),
    ("goal.start", true),
    ("goal.step_done", true),
    ("goal.cancel", true),
    ("goal.briefing", false),
    // quota scheduling
    ("quota.status", false),
    ("quota.tick", true),
    ("quota.route", false),
    ("quota.resume", true),
    // vm level
    ("vm.status", false),
    ("vm.list", false),
    ("vm.create", true),
    ("vm.start", true),
    ("vm.stop", true),
    ("vm.suspend", true),
    ("vm.resume", true),
    ("vm.destroy", true),
    ("vm.snapshot", true),
    ("vm.snapshot.delete", true),
    ("vm.fork", true),
    ("vm.transport", false),
    ("vm.template.list", false),
    ("vm.template.build", true),
    ("vm.template.delete", true),
];

/// Methods a pane token may never call: every mutation except claims (which an agent makes for
/// its own task), and the learned-policy list (it quotes command history).
pub const PANE_FORBIDDEN: &[&str] = &[
    "family.check",
    "task.best_of_n",
    "task.pick",
    "task.split",
    "policy.learned.list",
    "policy.learned.accept",
    "policy.learned.dismiss",
    "merge.queue.add",
    "merge.queue.cancel",
    "merge.queue.requeue",
    "merge.queue.run",
    "goal.create",
    "goal.plan",
    "goal.plan_submit",
    "goal.approve",
    "goal.start",
    "goal.step_done",
    "goal.cancel",
    "quota.tick",
    "quota.resume",
    "vm.create",
    "vm.start",
    "vm.stop",
    "vm.suspend",
    "vm.resume",
    "vm.destroy",
    "vm.snapshot",
    "vm.snapshot.delete",
    "vm.fork",
    "vm.template.build",
    "vm.template.delete",
];

// ---- config ------------------------------------------------------------------------------

fn overrides() -> &'static Mutex<HashMap<PathBuf, OrchestrateConfig>> {
    static O: OnceLock<Mutex<HashMap<PathBuf, OrchestrateConfig>>> = OnceLock::new();
    O.get_or_init(Mutex::default)
}

/// The `[orchestrate]` config (a test override for this server's state dir first).
pub fn cfg(server: &Server) -> OrchestrateConfig {
    if let Some(c) = overrides().lock().unwrap().get(&server.paths.state) {
        return c.clone();
    }
    load_cfg()
}

pub fn load_cfg() -> OrchestrateConfig {
    let cfg = vk_config::Config::load(vk_config::config_path())
        .map(|(c, _)| c)
        .unwrap_or_default();
    let (c, e) = OrchestrateConfig::from_toml(cfg.extra.get("orchestrate"));
    if let Some(e) = e {
        tracing::warn!(error = %e, "invalid [orchestrate] config; using defaults");
    }
    c
}

/// Use `c` instead of the config file for this server (tests).
pub fn set_cfg(server: &Server, c: OrchestrateConfig) {
    overrides().lock().unwrap().insert(server.paths.state.clone(), c);
}

/// `unsupported` naming the flag that turns a feature on.
pub fn disabled(feature: &str, flag: &str) -> vk_proto::rpc::RpcError {
    err(
        ErrorKind::Unsupported,
        format!("{feature} is off: set `[orchestrate.{flag}] enabled = true` in config.toml"),
    )
    .details(json!({"reason": "feature_disabled", "feature": feature, "flag": format!("orchestrate.{flag}.enabled")}))
}

pub fn require(on: bool, feature: &str, flag: &str) -> Result<(), vk_proto::rpc::RpcError> {
    if on { Ok(()) } else { Err(disabled(feature, flag)) }
}

// ---- store helpers ------------------------------------------------------------------------

pub(crate) fn put<T: Serialize>(tx: &mut Tx, kind: &'static str, id: &str, handle: Option<&str>, v: &T) {
    tx.m.put(kind, id, handle, v);
}

pub(crate) fn load_all<T: DeserializeOwned>(server: &Server, kind: &str) -> Vec<T> {
    server.with_core(|c| c.store.load(kind).unwrap_or_default())
}

pub(crate) fn load_one<T: DeserializeOwned>(server: &Server, kind: &str, id_or_handle: &str) -> Option<T> {
    server.with_core(|c| c.store.find(kind, id_or_handle).ok().flatten())
}

pub(crate) fn kv_get<T: DeserializeOwned + Default>(server: &Server, scope: &str, key: &str) -> T {
    server.with_core(|c| {
        c.store
            .kv_get(scope, key)
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    })
}

pub(crate) fn kv_put<T: Serialize>(tx: &mut Tx, scope: &str, key: &str, v: &T) {
    tx.m.kv(scope, key, serde_json::to_string(v).ok());
}

/// Next number of the named counter (`G` for goals, ...); the new value is written in `tx`.
pub(crate) fn next_counter(c: &mut Core, tx: &mut Tx, name: &str) -> u64 {
    let mut m: HashMap<String, u64> = c
        .store
        .kv_get("orch.counters", "all")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let n = m.entry(name.to_string()).or_default();
    *n += 1;
    let v = *n;
    tx.m.kv("orch.counters", "all", serde_json::to_string(&m).ok());
    v
}

/// Commit an event-only transaction.
pub fn emit(server: &Server, kind: &str, subject: Value, data: Value) {
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.event(kind, subject, data);
    let _ = server.commit(&mut c, tx);
}

/// The repository for a request: `repo` param, else the caller pane's cwd, else `.`.
pub(crate) fn repo_param(server: &Server, ctx: &Ctx, p: &Value) -> String {
    crate::api::s(p, "repo")
        .map(str::to_string)
        .or_else(|| {
            crate::api::resolve_pane(server, ctx, None)
                .ok()
                .and_then(|x| server.pane_cwd(&x.id))
        })
        .unwrap_or_else(|| ".".into())
}

/// Run `task.create` (or any API method) as the local user.
pub(crate) async fn call(server: &Arc<Server>, method: &str, params: Value) -> R {
    Box::pin(crate::api::dispatch(server, &crate::drafts::user_ctx(), method, &params)).await
}

pub(crate) fn from_orch(e: vk_orchestrate::Error) -> vk_proto::rpc::RpcError {
    match e {
        vk_orchestrate::Error::Invalid(m) => invalid(m),
        vk_orchestrate::Error::Refused(m) => err(ErrorKind::Conflict, m),
        other => internal(other),
    }
}

// ---- dispatch -----------------------------------------------------------------------------

/// Dispatch hook (one line in `api::dispatch`).
pub async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    // `task.create` with a string `agents` list (`--agents claude:2,codex:1`) is best-of-N.
    if method == "task.create" && p.get("agents").is_some_and(Value::is_string) {
        return Some(crate::orch_family::best_of_n(server, ctx, p).await);
    }
    if !METHODS.iter().any(|(m, _)| *m == method) {
        return None;
    }
    if method.starts_with("family.") || matches!(method, "task.best_of_n" | "task.compare" | "task.pick") {
        return crate::orch_family::api(server, ctx, method, p).await;
    }
    if method == "task.split" {
        return Some(crate::orch_split::split(server, ctx, p).await);
    }
    if method.starts_with("policy.learned.") {
        return crate::orch_learn::api(server, ctx, method, p).await;
    }
    if method.starts_with("task.claim") || method.starts_with("merge.") {
        return crate::orch_merge::api(server, ctx, method, p).await;
    }
    if method.starts_with("goal.") {
        return crate::orch_goal::api(server, ctx, method, p).await;
    }
    if method.starts_with("quota.") {
        return crate::orch_quota::api(server, ctx, method, p).await;
    }
    if method.starts_with("vm.") {
        return crate::orch_vm::api(server, ctx, method, p).await;
    }
    None
}

/// Background work, started once at server start: family checks, conflict prediction, goal
/// progress and the quota tick. Each part checks its own flag on every pass, so the loop costs
/// one config read per pass while everything is off.
pub fn start(server: &Arc<Server>) {
    static STARTED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    {
        let mut s = STARTED.lock().unwrap();
        if s.contains(&server.paths.state) {
            return;
        }
        s.push(server.paths.state.clone());
    }
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let weak = Arc::downgrade(server);
    handle.spawn(async move {
        let mut last_predict = std::time::Instant::now();
        let mut last_quota = std::time::Instant::now();
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            let Some(srv) = weak.upgrade() else { return };
            let c = cfg(&srv);
            if c.best_of_n.enabled {
                crate::orch_family::tick(&srv, &c).await;
            }
            if c.merge.enabled && last_predict.elapsed() >= c.merge.predict_every() {
                last_predict = std::time::Instant::now();
                crate::orch_merge::tick(&srv, &c).await;
            }
            if c.planner.enabled {
                crate::orch_goal::tick(&srv, &c).await;
            }
            if c.quota.enabled && last_quota.elapsed() >= c.quota.interval() {
                last_quota = std::time::Instant::now();
                crate::orch_quota::tick_apply(&srv, &c, false).await;
            }
        }
    });
}
