//! Opt-in background assistance (14 §10, A3): coalesced summaries and possible-stall notices.
//!
//! Nothing here exists unless the user turned it on. The sweeper task is only spawned while
//! `[assistant] enabled`, `background_enabled` and at least one of `background_summaries` /
//! `stall_notices` are set (an idle server keeps no extra timer), re-checks that on every wake
//! and exits when it no longer holds. A sweep:
//!
//! 1. gathers cheap deterministic views (a fingerprint per workspace, the failing-command tail
//!    per live run) and asks the pure planner ([`vk_assist::background`]) what is due;
//! 2. for each planned action creates a request through the same `generate` path as an
//!    interactive one, **only when both the config's and the workspace consent's `auto_send`
//!    list the operation** (a background request has nobody to confirm a preview; without
//!    that grant the action is skipped and reported), using the `background` profile and the
//!    background scheduling class, coalesced per workspace.
//!
//! Results are passive: a stall notice becomes a notification (never focus, never over a
//! pane); a summary is just a stored result. Nothing is ever answered, sent or changed.

use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use vk_assist::background::{self as bgp, Action, RunView, WorkspaceView};
use vk_assist::config::Purpose;
use vk_assist::stall;

#[derive(Default)]
pub struct Runtime {
    running: AtomicBool,
}

impl Runtime {
    pub fn running_now(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
}

pub fn system_ctx() -> Ctx {
    Ctx {
        client_id: "assistant-background".into(),
        kind: "system".into(),
        pane_scope: None,
        remote: false,
    }
}

fn wanted(cfg: &AssistConfig) -> bool {
    cfg.background_active() && (cfg.background_summaries || cfg.stall_notices)
}

/// Spawn the sweeper if the opt-ins hold and it is not already running.
pub fn ensure(server: &Arc<Server>) {
    let Ok((cfg, _)) = load_config() else {
        return;
    };
    if !wanted(&cfg) {
        return;
    }
    let st = state(server);
    if st
        .bg
        .running
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    let srv = server.clone();
    tokio::spawn(async move {
        loop {
            let period = load_config()
                .map(|(c, _)| c.background_interval_seconds.clamp(30, 86_400))
                .unwrap_or(300);
            tokio::time::sleep(Duration::from_secs(period)).await;
            match load_config() {
                Ok((c, _)) if wanted(&c) => {}
                _ => {
                    state(&srv).bg.running.store(false, Ordering::SeqCst);
                    return;
                }
            }
            let _ = tick(&srv).await;
        }
    });
}

/// Called once at server start.
pub fn start(server: &Arc<Server>) {
    ensure(server);
}

/// Workspace ids with an activity flag, and live (run, workspace) pairs.
type Raw = (Vec<(String, bool)>, Vec<(String, String)>);

fn collect(server: &Server) -> (Vec<WorkspaceView>, Vec<(String, String)>) {
    // (workspace ids with an activity flag, live (run, workspace) pairs)
    let (ws, runs): Raw = server.with_core(|c| {
        let mut ws = vec![];
        for w in &c.model.workspaces {
            let panes: Vec<&str> = c
                .model
                .panes
                .iter()
                .filter(|p| p.workspace == w.id)
                .map(|p| p.id.as_str())
                .collect();
            let active =
                c.model.interactions.iter().any(|i| {
                    panes.contains(&i.pane.as_str()) && i.status == InteractionStatus::Open
                }) || c
                    .model
                    .runs
                    .iter()
                    .any(|r| panes.contains(&r.pane.as_str()) && r.ended_at_ms.is_none());
            ws.push((w.id.clone(), active));
        }
        let runs = c
            .model
            .runs
            .iter()
            .filter(|r| r.ended_at_ms.is_none())
            .filter_map(|r| c.pane(&r.pane).map(|p| (r.id.clone(), p.workspace.clone())))
            .collect();
        (ws, runs)
    });
    let views = ws
        .into_iter()
        .map(|(id, active)| WorkspaceView {
            fingerprint: stale::fingerprint(server, "workspace", &id).unwrap_or_default(),
            id,
            active,
        })
        .collect();
    (views, runs)
}

fn run_obs(server: &Server, run: &str) -> Vec<stall::Obs> {
    crate::tracking::items_of(server, run)
        .into_iter()
        .filter_map(|r| {
            r.command.map(|command| stall::Obs {
                command,
                exit_code: r.exit_code,
                ended_at_ms: r.ended_at_ms,
            })
        })
        .collect()
}

/// Create the request for one planned action, or say why it was skipped.
async fn execute(server: &Arc<Server>, cfg: &AssistConfig, a: &Action) -> Result<String, String> {
    let (op, ws_id, extra, key) = match a {
        Action::Summary {
            workspace,
            fingerprint,
        } => (
            Operation::BackgroundSummary,
            workspace.clone(),
            json!({}),
            format!("bg-summary-{workspace}-{fingerprint}"),
        ),
        Action::Stall {
            run,
            workspace,
            signal,
        } => (
            Operation::StallNotice,
            workspace.clone(),
            json!({"run": run}),
            format!("bg-stall-{run}-{}", signal.digest()),
        ),
    };
    let ws = server
        .with_core(|c| c.ws(&ws_id).cloned())
        .ok_or_else(|| "the workspace is gone".to_string())?;
    let resolved = cfg
        .resolve_for(Purpose::Background, None)
        .map_err(|e| e.message)?;
    let grants = vk_assist::consent::load(&consent_path());
    let grant = vk_assist::consent::check(
        &grants,
        &canonical(&ws.root_path),
        &resolved,
        op.as_str(),
        op.classes(),
    )
    .map_err(|e| e.message)?;
    // Nobody is there to confirm a preview: both lists must name the operation.
    if !cfg.auto_send_allows(op.as_str()) || !grant.auto_send.iter().any(|o| o == op.as_str()) {
        return Err(
            "auto_send_not_granted: background requests need the operation in both [assistant] auto_send and the workspace consent".into(),
        );
    }
    let mut params = json!({
        "operation": op.as_str(),
        "priority": "background",
        "workspace": ws.id,
        "idempotency_key": key,
    });
    if let (Some(o), Some(x)) = (params.as_object_mut(), extra.as_object()) {
        for (k, v) in x {
            o.insert(k.clone(), v.clone());
        }
    }
    let r = generate(server, &system_ctx(), &params)
        .await
        .map_err(|e| e.message)?;
    let id = r["request"]["id"].as_str().unwrap_or("").to_string();
    if r["requires_confirmation"] == true {
        // Never leave a preview behind that only a human could send.
        if let Ok(req) = find(server, &id) {
            let _ = cancel_one(
                server,
                req,
                Category::Cancelled,
                "background requests are never previewed",
            );
        }
        return Err("auto_send_not_granted: the request needs a confirmation".into());
    }
    Ok(id)
}

/// One sweep. Returns what was created or skipped.
pub(super) async fn tick(server: &Arc<Server>) -> Result<Value, RpcError> {
    let (cfg, _) = config()?;
    if !wanted(&cfg) {
        return Ok(json!({"active": false, "actions": []}));
    }
    let (wviews, runs) = collect(server);
    let rviews: Vec<RunView> = runs
        .iter()
        .map(|(id, ws)| RunView {
            id: id.clone(),
            workspace: ws.clone(),
            obs: run_obs(server, id),
        })
        .collect();
    let mut st = data::bg_state(server);
    let t = now();
    let planned = bgp::plan(&cfg, t, &st, &wviews, &rviews);
    let mut results = vec![];
    for a in &planned {
        let (kind, subject) = match a {
            Action::Summary { workspace, .. } => ("summary", workspace.clone()),
            Action::Stall { run, .. } => ("stall", run.clone()),
        };
        match execute(server, &cfg, a).await {
            Ok(id) => {
                st.record(a, t);
                results.push(json!({"action": kind, "subject": subject, "request": id}));
            }
            Err(why) => results.push(json!({"action": kind, "subject": subject, "skipped": why})),
        }
    }
    st.retain(
        &wviews.iter().map(|w| w.id.clone()).collect::<Vec<_>>(),
        &runs.iter().map(|r| r.0.clone()).collect::<Vec<_>>(),
    );
    data::store_bg_state(server, &st);
    Ok(json!({"active": true, "actions": results}))
}

pub(super) async fn api(server: &Arc<Server>, p: &Value) -> R {
    match s(p, "action").unwrap_or("status") {
        "status" => {
            let (cfg, _) = config()?;
            let st = data::bg_state(server);
            Ok(json!({
                "enabled": cfg.enabled,
                "background_enabled": cfg.background_enabled,
                "active": wanted(&cfg),
                "summaries": cfg.background_summaries,
                "stall_notices": cfg.stall_notices,
                "interval_seconds": cfg.background_interval_seconds,
                "stall_repeat_threshold": cfg.stall_repeat_threshold,
                "sweeper_running": state(server).bg.running.load(Ordering::SeqCst),
                "summarized": st.summarized.len(),
                "notified_runs": st.notified.len(),
                "note": "background requests need the operation in both [assistant] auto_send and the workspace consent",
            }))
        }
        "tick" => tick(server).await,
        other => Err(invalid(format!("unknown action `{other}` (status | tick)"))),
    }
}
