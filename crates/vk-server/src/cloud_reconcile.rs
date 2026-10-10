//! The cloud box reconciler (spec 17 §6.2). It lists every signed-in provider, merges the
//! listing with the box records (kv `cloud_box`), computes each box's ownership, commits what
//! changed (emitting `cloud.box.changed`) and applies the idle policies:
//!
//! - `[cloud] idle_suspend_after` suspends an idle box (providers with an explicit suspend;
//!   Sprites sleep on their own);
//! - `[cloud] idle_destroy_after` destroys an idle box, but only when nothing in it is unsynced,
//!   and never with force.
//!
//! It runs at server start, every 10 minutes, and on `cloud.box.list {refresh: true}` /
//! `cloud.prune`.

use crate::Server;
use crate::sandbox::cloud::{self as cl, BoxRecord};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use vk_cloud::{BoxState, CloudError, Provider, Secret};

/// Time between background runs.
pub const INTERVAL: Duration = Duration::from_secs(600);
/// Delay of the first run after server start.
const FIRST_RUN: Duration = Duration::from_secs(5);

/// One run at a time (a refresh request waits for a running pass).
static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();

/// Start the background loop (server start).
pub fn start(server: &Arc<Server>) {
    let Ok(h) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let weak = Arc::downgrade(server);
    h.spawn(async move {
        tokio::time::sleep(FIRST_RUN).await;
        loop {
            let Some(srv) = weak.upgrade() else { return };
            let errors = run(&srv, None).await;
            for e in errors {
                tracing::debug!(error = %e, "cloud reconcile");
            }
            drop(srv);
            tokio::time::sleep(INTERVAL).await;
        }
    });
}

/// Facts the ownership of one box is computed from.
#[derive(Debug, Clone, Copy, Default)]
pub struct OwnIn<'a> {
    /// This host's tag.
    pub our_host: &'a str,
    /// The box's host tag, when it has one.
    pub box_host: Option<&'a str>,
    /// The provider listed the box in this pass.
    pub listed: bool,
    /// A record of this host created the box for a task ([`BoxRecord::ours`]).
    pub ours: bool,
    /// That task still exists and is active.
    pub task_exists: bool,
    /// The task has panes in the box, or the box has active sessions.
    pub live: bool,
    /// Seconds since the last activity.
    pub idle_for_s: u64,
    /// `idle` after this many seconds without activity (`None`: never).
    pub idle_after_s: Option<u64>,
}

/// `attached | idle | orphaned | foreign | missing` (spec 17 §6.2).
pub fn ownership(i: &OwnIn) -> &'static str {
    if !i.listed {
        return "missing";
    }
    if i.box_host.is_some_and(|h| h != i.our_host) {
        return "foreign";
    }
    if !i.ours || !i.task_exists {
        return "orphaned";
    }
    if i.live {
        return "attached";
    }
    match i.idle_after_s {
        Some(a) if i.idle_for_s >= a => "idle",
        _ => "attached",
    }
}

/// The earlier of the two idle thresholds, in seconds.
pub fn idle_after(cfg: &vk_cloud::CloudConfig) -> Option<u64> {
    match (cfg.idle_suspend(), cfg.idle_destroy()) {
        (Some(a), Some(b)) => Some(a.min(b).as_secs()),
        (a, b) => a.or(b).map(|d| d.as_secs()),
    }
}

fn error_json(provider: &str, e: &CloudError) -> Value {
    json!({
        "provider": provider,
        "kind": serde_json::to_value(e.kind).unwrap_or(Value::Null),
        "message": e.message,
    })
}

/// One pass over the signed-in providers (all, or only `only`). Returns the per-provider errors
/// (`{provider, kind, message}`), as `cloud.box.list` reports them.
pub async fn run(server: &Arc<Server>, only: Option<&str>) -> Vec<Value> {
    let _g = LOCK.get_or_init(Default::default).lock().await;
    let cfg = cl::cloud_cfg();
    let kc = match cl::keychain() {
        Ok(k) => k,
        Err(e) => {
            return vec![
                json!({"provider": only.unwrap_or("*"), "kind": "invalid_params", "message": e.message}),
            ];
        }
    };
    let our_host = vk_cloud::naming::host_tag(&cl::host_id(server));
    let idle_after_s = idle_after(&cfg);
    let mut errors = Vec::new();
    for p in vk_cloud::providers(&cfg) {
        if only.is_some_and(|o| o != p.id()) {
            continue;
        }
        let cred = match vk_cloud::auth::resolve(p.as_ref(), &cfg, &kc, &cl::host_env) {
            Ok(Some((s, _))) => s,
            Ok(None) => continue,
            Err(e) => {
                errors.push(error_json(p.id(), &e));
                continue;
            }
        };
        let listed = match p.list(&cred).await {
            Ok(l) => l,
            Err(e) => {
                errors.push(error_json(p.id(), &e));
                continue;
            }
        };
        let records: HashMap<String, BoxRecord> = cl::list_records(server)
            .into_iter()
            .filter(|r| r.provider == p.id())
            .map(|r| (r.box_ref(), r))
            .collect();
        let mut seen = HashSet::new();
        for rb in &listed {
            let box_ref = format!("{}/{}", p.id(), rb.id);
            seen.insert(box_ref.clone());
            let old = records.get(&box_ref).cloned();
            let mut rec = old
                .clone()
                .unwrap_or_else(|| crate::cloud_api::record_from_remote(rb));
            merge_remote(&mut rec, rb);
            let (task_exists, panes) = server.with_core(|c| {
                let exists = rec
                    .task
                    .as_deref()
                    .and_then(|t| c.task(t))
                    .is_some_and(|t| t.status == "active");
                (exists, cl::cloud_panes(c, rec.task.as_deref()).1)
            });
            let mut sessions = 0;
            if rec.ours()
                && rb.state == BoxState::Running
                && let Ok(s) = p.sessions(&cred, &rb.id).await
            {
                sessions = s.iter().filter(|s| s.active).count() as u32;
            }
            let now = vk_cloud::now_s();
            let live = !panes.is_empty() || sessions > 0;
            rec.sessions = sessions;
            rec.panes = panes;
            if live {
                rec.last_activity_at = now;
            } else {
                rec.last_activity_at = rec
                    .last_activity_at
                    .max(rb.last_active_at)
                    .max(rec.created_at);
            }
            let idle_for_s = now.saturating_sub(rec.last_activity_at);
            rec.ownership = ownership(&OwnIn {
                our_host: &our_host,
                box_host: rec.tags.as_ref().map(|t| t.host.as_str()),
                listed: true,
                ours: rec.ours(),
                task_exists,
                live,
                idle_for_s,
                idle_after_s,
            })
            .to_string();
            if old.as_ref() != Some(&rec) {
                cl::save_record(server, &rec);
            }
            if rec.ownership == "idle" {
                apply_idle(server, p.as_ref(), &cred, &mut rec, idle_for_s, &cfg).await;
            }
        }
        for (box_ref, mut rec) in records {
            if seen.contains(&box_ref) || rec.ownership == "missing" {
                continue;
            }
            rec.ownership = "missing".into();
            cl::save_record(server, &rec);
        }
    }
    errors
}

/// Provider facts into the record.
fn merge_remote(rec: &mut BoxRecord, rb: &vk_cloud::RemoteBox) {
    rec.name = rb.name.clone();
    rec.state = rb.state.as_str().to_string();
    if rb.url.is_some() {
        rec.url = rb.url.clone();
    }
    if rb.tags.is_some() {
        rec.tags = rb.tags.clone();
    }
    if rec.created_at == 0 {
        rec.created_at = rb.created_at;
    }
}

/// The idle policies (spec 17 §6.2). Destroy never forces: unsynced work keeps the box.
async fn apply_idle(
    server: &Arc<Server>,
    p: &dyn Provider,
    cred: &Secret,
    rec: &mut BoxRecord,
    idle_for_s: u64,
    cfg: &vk_cloud::CloudConfig,
) {
    if let Some(d) = cfg.idle_destroy()
        && idle_for_s >= d.as_secs()
    {
        match crate::cloud_api::destroy_record(server, rec, false).await {
            Ok(()) => return,
            Err(e) => {
                tracing::info!(box_ref = %rec.box_ref(), reason = %e.message, "idle cloud box kept")
            }
        }
    }
    if let Some(d) = cfg.idle_suspend()
        && idle_for_s >= d.as_secs()
        && p.caps().explicit_suspend
        && rec.state == BoxState::Running.as_str()
        && p.suspend(cred, &rec.id).await.is_ok()
    {
        rec.state = match p.get(cred, &rec.id).await {
            Ok(rb) => rb.state.as_str().to_string(),
            Err(_) => BoxState::Paused.as_str().to_string(),
        };
        cl::save_record(server, rec);
    }
}
