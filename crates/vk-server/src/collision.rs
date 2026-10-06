//! The shared-cwd collision tracker and advisory claims (05 §10; lane 3A). Pure rules live in
//! `vk_tasks::collision`; this module owns the state, the signals and the API.
//!
//! **Advisory.** Vibeke warns; it never blocks, reverts or reassigns changes on the basis of
//! attribution. A `file_change` item is evidence that a tool attempted or reported an edit, not
//! proof of who owns a file's content.
//!
//! Signals (all end in a [`vk_tasks::collision::Touch`] of one repo root):
//! 1. adapter reports (`PostToolUse` of Edit/Write/MultiEdit/NotebookEdit, pi/omp `edit`/`write`,
//!    Codex `apply_patch`, OpenCode `file.edited`): authoritative for that run; Read tool events
//!    feed the "edited what another run read" rule;
//! 2. the file-system watcher ([`watch`]), attributed (a) to a run with an in-flight reported
//!    tool call on that path, (b) with `fs_attribution = "aggressive"` to a run whose process
//!    held the file open for writing (Linux `/proc/*/fd`, best effort), (c) to the runs working
//!    in that checkout, `ambiguous` when more than one;
//! 3. `git status --porcelain` polled every `poll_interval` while an agent in the repo works.
//!
//! The watcher and the poll only run for a checkout two or more live runs share (or one with a
//! claim): a lone agent cannot collide, and an idle machine pays nothing.
//!
//! Records (`collision` entities) group findings by repo root and run set; `task.collision_detected`
//! is emitted when a record is created, gains a path or a run, or its severity rises, and a
//! notification fires once per path set per window (`high` and `medium` only; `low` is a sidebar
//! hint). Actions ([`act`]): pause a run, tell the runs (native steer only, never typed into a TUI),
//! start a fresh task from the shared checkout's `HEAD`, ignore a path. Claims: `task.claim`,
//! enforced for cooperating adapters only with `collision.enforce_claims`.

mod act;
pub mod watch;

use crate::Server;
use crate::agents::harness::{Family, Harness};
use crate::api::{Ctx, R, err, internal, invalid, not_found, req, s};
use crate::core::Tx;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use vk_proto::model::{AgentRun, Execution, Task};
use vk_proto::rpc::ErrorKind;
use vk_store::now_ms;
use vk_tasks::collision as vc;

pub const METHODS: &[(&str, bool)] = &[
    ("collision.list", false),
    ("collision.get", false),
    ("collision.status", false),
    ("collision.ignores", false),
    ("collision.ignore", true),
    ("collision.unignore", true),
    ("collision.pause", true),
    ("collision.tell", true),
    ("collision.start_task", true),
    ("task.claim", true),
    ("task.claims", false),
    ("task.claim_release", true),
];

/// Full scope only: silencing, pausing, steering and splitting are the user's decisions; an agent
/// may *see* collisions of its own run and manage its own claims.
pub const PANE_FORBIDDEN: &[&str] = &[
    "collision.status",
    "collision.ignores",
    "collision.ignore",
    "collision.unignore",
    "collision.pause",
    "collision.tell",
    "collision.start_task",
];

pub const K_COLLISION: &str = "collision";
pub const K_CLAIM: &str = "claim";
pub const K_IGNORE: &str = "collision_ignore";

/// Claims one run may hold, and the total kept.
const MAX_CLAIMS_PER_RUN: usize = 100;
const MAX_CLAIMS: usize = 1000;
/// Closed collision records listed at most.
const MAX_HISTORY: usize = 200;
/// A reported edit tool call counts as "in flight" this long after it ended (the watcher's event
/// for its write may arrive after the report).
pub(crate) const IN_FLIGHT_GRACE_MS: i64 = 3000;
/// A tool call that never reported its end is forgotten after this.
const IN_FLIGHT_MAX_MS: i64 = 600_000;

// ---- state ----------------------------------------------------------------------------------

/// A path pattern the user chose to ignore ("Ignore for this path").
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ignore {
    pub id: String,
    pub root: String,
    pub path: String,
    pub created_ms: i64,
    #[serde(default)]
    pub expires_ms: Option<i64>,
    pub by: String,
}

#[derive(Clone, Debug)]
pub(crate) struct InFlight {
    pub run: String,
    pub root: String,
    pub path: String,
    pub tool_use: Option<String>,
    pub started_ms: i64,
    pub ended_ms: Option<i64>,
}

#[derive(Default)]
pub(crate) struct Root {
    pub tracker: vc::Tracker,
    pub git_prev: Option<std::collections::BTreeMap<String, String>>,
    pub last_poll_ms: i64,
}

#[derive(Default)]
pub(crate) struct Inner {
    pub roots: HashMap<String, Root>,
    pub open: Vec<vc::CollisionRec>,
    pub claims: Vec<vc::Claim>,
    pub ignores: Vec<Ignore>,
    pub in_flight: Vec<InFlight>,
    /// Open records a previous server left behind (closed by [`start`]).
    pub stale: Vec<vc::CollisionRec>,
}

#[derive(Default)]
pub struct State {
    pub(crate) inner: Mutex<Inner>,
    loaded: AtomicBool,
    cfg_override: Mutex<Option<vk_config::Collision>>,
    cfg_cache: Mutex<Option<(Instant, vk_config::Collision)>>,
    /// Steering text waiting for a hook to carry it (run → texts with their queue time).
    pub(crate) pending_ctx: Mutex<HashMap<String, Vec<(i64, String)>>>,
    pub(crate) watch: watch::Watch,
}

/// The live `[collision]` config (cached for two seconds; tests override it).
pub fn config(server: &Server) -> vk_config::Collision {
    if let Some(c) = server.collision.cfg_override.lock().unwrap().clone() {
        return c;
    }
    let mut g = server.collision.cfg_cache.lock().unwrap();
    if let Some((at, c)) = g.as_ref()
        && at.elapsed() < Duration::from_secs(2)
    {
        return c.clone();
    }
    let c = crate::config_api::current().collision();
    *g = Some((Instant::now(), c.clone()));
    c
}

/// Replace the config (tests).
#[cfg(test)]
pub(crate) fn set_config(server: &Server, cfg: vk_config::Collision) {
    *server.collision.cfg_override.lock().unwrap() = Some(cfg);
}

pub(crate) fn rules(cfg: &vk_config::Collision) -> vc::Rules {
    vc::Rules {
        window_ms: cfg.window.as_millis() as i64,
        read_window_ms: cfg.read_window.as_millis() as i64,
        dir_depth: cfg.dir_depth,
        max_touches: cfg.max_touches,
    }
}

/// Load claims and ignores from the store once. Open collision records of a previous server are
/// parked in `stale` for [`start`] to close.
pub(crate) fn ensure_loaded(server: &Server) {
    let st = &server.collision;
    if st.loaded.load(Ordering::Acquire) {
        return;
    }
    let (open, claims, ignores): (Vec<vc::CollisionRec>, Vec<vc::Claim>, Vec<Ignore>) = server
        .with_core(|c| {
            (
                c.store.load(K_COLLISION).unwrap_or_default(),
                c.store.load(K_CLAIM).unwrap_or_default(),
                c.store.load(K_IGNORE).unwrap_or_default(),
            )
        });
    let mut g = st.inner.lock().unwrap();
    if st.loaded.swap(true, Ordering::AcqRel) {
        return;
    }
    g.claims = claims;
    g.ignores = ignores;
    g.stale = open;
}

fn new_id(prefix: &str) -> String {
    format!(
        "{prefix}_{}",
        crate::core::ulid()[16..].to_ascii_lowercase()
    )
}

// ---- runs and roots -------------------------------------------------------------------------

pub(crate) fn run_alive(r: &AgentRun) -> bool {
    r.ended_at_ms.is_none() && r.execution.value != Execution::Exited
}

pub(crate) fn live_runs(server: &Server) -> Vec<AgentRun> {
    server.with_core(|c| {
        c.model
            .runs
            .iter()
            .filter(|r| run_alive(r))
            .cloned()
            .collect()
    })
}

/// The checkout (git work tree root) a run works in: its cwd, else its pane's, else its task's.
pub(crate) fn root_of(server: &Server, run: &AgentRun) -> Option<String> {
    let cwd = run
        .cwd
        .clone()
        .or_else(|| server.pane_cwd(&run.pane))
        .or_else(|| server.with_core(|c| c.pane(&run.pane).and_then(|p| p.cwd.clone())))
        .or_else(|| {
            server.with_core(|c| {
                run.task.as_ref().and_then(|t| c.task(t)).map(|t| {
                    t.worktree_path
                        .clone()
                        .unwrap_or_else(|| t.repo_root.clone())
                })
            })
        })?;
    if cwd.is_empty() {
        return None;
    }
    Some(
        vc::repo_root_of(Path::new(&cwd))
            .to_string_lossy()
            .into_owned(),
    )
}

pub(crate) fn run_label(r: &AgentRun) -> String {
    r.name.clone().unwrap_or_else(|| r.harness.clone())
}

fn run_by_id(server: &Server, id: &str) -> Option<AgentRun> {
    server.with_core(|c| c.run(id).cloned())
}

// ---- signals from adapters ------------------------------------------------------------------

fn ignored(inner: &Inner, root: &str, path: &str, now: i64) -> bool {
    inner.ignores.iter().any(|i| {
        i.root == root && i.expires_ms.is_none_or(|e| e > now) && vc::glob_match(&i.path, path)
    })
}

/// A hook event of a run (`on_signal`). Cheap for everything that is not an edit or a read.
pub fn observe(server: &Arc<Server>, run: &AgentRun, event: &str, p: &Value) {
    if !matches!(event, "PreToolUse" | "PostToolUse" | "PostToolUseFailure") {
        return;
    }
    let tool = p.get("tool_name").and_then(Value::as_str).unwrap_or("");
    let input = p.get("tool_input").cloned().unwrap_or(Value::Null);
    let edits = vc::edit_paths(tool, &input);
    let reads = vc::read_paths(tool, &input);
    if edits.is_empty() && reads.is_empty() {
        return;
    }
    let cfg = config(server);
    if !cfg.enabled {
        return;
    }
    let Some(root) = root_of(server, run) else {
        return;
    };
    ensure_loaded(server);
    let now = now_ms();
    let tool_use = p
        .get("tool_use_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let rel = |path: &str| {
        vc::relativize(Path::new(&root), path).filter(|r| !vc::is_ignored_path(r, &cfg.ignore))
    };
    match event {
        "PreToolUse" => {
            let mut g = server.collision.inner.lock().unwrap();
            for (path, _) in &edits {
                if let Some(r) = rel(path) {
                    g.in_flight.push(InFlight {
                        run: run.id.clone(),
                        root: root.clone(),
                        path: r,
                        tool_use: tool_use.clone(),
                        started_ms: now,
                        ended_ms: None,
                    });
                }
            }
        }
        "PostToolUseFailure" => {
            let mut g = server.collision.inner.lock().unwrap();
            g.in_flight
                .retain(|f| !(f.run == run.id && f.tool_use == tool_use && f.ended_ms.is_none()));
        }
        _ => {
            {
                let mut g = server.collision.inner.lock().unwrap();
                for f in g
                    .in_flight
                    .iter_mut()
                    .filter(|f| f.run == run.id && f.ended_ms.is_none())
                {
                    if tool_use.is_none() || f.tool_use == tool_use {
                        f.ended_ms = Some(now);
                    }
                }
            }
            for (path, op) in &edits {
                if let Some(r) = rel(path) {
                    record(
                        server,
                        &cfg,
                        &root,
                        vc::Touch::write(&run.id, &r, now, vc::Source::Adapter, op.as_str()),
                    );
                }
            }
            for path in &reads {
                if let Some(r) = rel(path) {
                    record(server, &cfg, &root, vc::Touch::read(&run.id, &r, now));
                }
            }
        }
    }
}

/// An adapter reported a changed file outside a tool call (OpenCode `file.edited`).
pub fn file_changed(server: &Arc<Server>, run: &AgentRun, path: &str, op: &str) {
    let cfg = config(server);
    if !cfg.enabled {
        return;
    }
    let Some(root) = root_of(server, run) else {
        return;
    };
    ensure_loaded(server);
    let Some(rel) =
        vc::relativize(Path::new(&root), path).filter(|r| !vc::is_ignored_path(r, &cfg.ignore))
    else {
        return;
    };
    record(
        server,
        &cfg,
        &root,
        vc::Touch::write(&run.id, &rel, now_ms(), vc::Source::Adapter, op),
    );
}

/// Record a touch of `root` and turn what the rules say into records, events and notifications.
pub(crate) fn record(server: &Server, cfg: &vk_config::Collision, root: &str, touch: vc::Touch) {
    let now = touch.at_ms;
    let findings = {
        let mut g = server.collision.inner.lock().unwrap();
        if ignored(&g, root, &touch.path, now) {
            return;
        }
        let claims: Vec<vc::Claim> = g
            .claims
            .iter()
            .filter(|c| c.root == root)
            .cloned()
            .collect();
        g.roots.entry(root.to_string()).or_default().tracker.record(
            &rules(cfg),
            &claims,
            touch.clone(),
        )
    };
    for f in findings {
        apply_finding(server, cfg, root, &touch, f);
    }
}

fn apply_finding(
    server: &Server,
    cfg: &vk_config::Collision,
    root: &str,
    touch: &vc::Touch,
    f: vc::Finding,
) {
    let now = touch.at_ms;
    let window_ms = cfg.window.as_millis() as i64;
    let (rec, merge, notify) = {
        let mut g = server.collision.inner.lock().unwrap();
        let idx = g.open.iter().position(|r| r.accepts(root, &f));
        let mut rec = match idx {
            Some(i) => g.open.remove(i),
            None => vc::CollisionRec::new(&new_id("col"), root, now),
        };
        let merge = rec.merge(&f);
        // The timeline starts with what the window already holds for the new paths.
        let rl = rules(cfg);
        if let Some(rs) = g.roots.get(root) {
            for p in &merge.new_paths {
                for t in rs.tracker.timeline(&rl, now, p) {
                    if t != *touch {
                        rec.note(&t);
                    }
                }
            }
        }
        rec.note(touch);
        let notify = cfg.notify
            && merge.changed()
            && rec.severity >= vc::Severity::Medium
            && rec.should_notify(now, window_ms);
        g.open.push(rec.clone());
        (rec, merge, notify)
    };
    if !merge.changed() && !notify {
        // A repeat touch of known paths: the in-memory timeline grows, nothing is written until
        // the record changes or closes.
        return;
    }
    let data = json!({
        "repo": rec.root,
        "severity": rec.severity.as_str(),
        "reason": f.reason.kind(),
        "paths": rec.paths.iter().map(|h| h.path.clone()).collect::<Vec<_>>(),
        "new_paths": merge.new_paths,
        "runs": rec.runs,
        "new_runs": merge.new_runs,
        "ambiguous": rec.ambiguous,
        "created": merge.created,
        "raised": merge.severity_raised,
    });
    persist(server, &rec, Some(("task.collision_detected", data)));
    if notify {
        notify_collision(server, &rec);
    }
}

/// Store the record and optionally emit an event in the same transaction.
fn persist(server: &Server, rec: &vc::CollisionRec, event: Option<(&str, Value)>) {
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    match rec.status {
        vc::Status::Open => tx.m.put(K_COLLISION, &rec.id, None, rec),
        _ => tx.m.close(K_COLLISION, &rec.id, None, rec),
    };
    if let Some((kind, data)) = event {
        tx.event(kind, json!({"collision": rec.id}), data);
    }
    let _ = server.commit(&mut c, tx);
}

pub(crate) fn headline(rec: &vc::CollisionRec) -> String {
    let n = rec.runs.len();
    let paths = rec.headline_paths(1);
    let first = paths.first().cloned().unwrap_or_default();
    let more = rec.paths.len().saturating_sub(1);
    let what = if rec.ambiguous {
        "possibly editing"
    } else {
        "editing"
    };
    if more > 0 {
        format!("{n} agents {what} {first} (+{more} more)")
    } else {
        format!("{n} agents {what} {first}")
    }
}

fn notify_collision(server: &Server, rec: &vc::CollisionRec) {
    let runs: Vec<AgentRun> = server.with_core(|c| {
        rec.runs
            .iter()
            .filter_map(|id| c.run(id).cloned())
            .collect()
    });
    let names: Vec<String> = runs
        .iter()
        .map(|r| format!("{} ({})", run_label(r), r.handle))
        .collect();
    let pane = runs.last().map(|r| r.pane.clone());
    let urgency = if rec.severity == vc::Severity::High {
        "normal"
    } else {
        "low"
    };
    let body = format!(
        "{} — advisory: open the collision view to pause, tell or split.",
        if names.is_empty() {
            "agents".to_string()
        } else {
            names.join(" and ")
        }
    );
    server.notify("collision", pane.as_deref(), &headline(rec), &body, urgency);
}

/// Close a record (`cleared` or `ignored`) and emit `task.collision_cleared`.
pub(crate) fn clear_record(server: &Server, mut rec: vc::CollisionRec, reason: &str, now: i64) {
    if rec.status == vc::Status::Open {
        rec.status = vc::Status::Cleared;
    }
    rec.cleared_ms = Some(now);
    rec.cleared_reason = Some(reason.to_string());
    persist(
        server,
        &rec,
        Some((
            "task.collision_cleared",
            json!({"repo": rec.root, "reason": reason, "severity": rec.severity.as_str(), "runs": rec.runs, "paths": rec.paths.iter().map(|h| h.path.clone()).collect::<Vec<_>>()}),
        )),
    );
}

// ---- housekeeping ---------------------------------------------------------------------------

/// Server start: close records a previous server left open, start the watcher worker.
pub fn start(server: &Arc<Server>) {
    ensure_loaded(server);
    let stale = std::mem::take(&mut server.collision.inner.lock().unwrap().stale);
    for rec in stale {
        clear_record(server, rec, "restart", now_ms());
    }
    watch::spawn(server);
}

/// The periodic sweep (also run by [`watch`]'s worker): clear quiet records and records with
/// fewer than two live runs, forget touches and in-flight calls of ended runs, release their
/// claims, drop expired ignores. Returns the number of records cleared.
pub(crate) fn sweep(server: &Server, now: i64) -> usize {
    ensure_loaded(server);
    let cfg = config(server);
    let window_ms = cfg.window.as_millis() as i64;
    let live: std::collections::HashSet<String> =
        live_runs(server).into_iter().map(|r| r.id).collect();
    let rl = rules(&cfg);
    let (cleared, released, expired) = {
        let mut g = server.collision.inner.lock().unwrap();
        let mut cleared = Vec::new();
        let mut keep = Vec::new();
        for rec in std::mem::take(&mut g.open) {
            let alive = rec.runs.iter().filter(|r| live.contains(*r)).count();
            if now.saturating_sub(rec.last_ms) > window_ms {
                cleared.push((rec, "quiet"));
            } else if alive < 2 {
                cleared.push((rec, "runs_ended"));
            } else {
                keep.push(rec);
            }
        }
        g.open = keep;
        g.in_flight.retain(|f| {
            live.contains(&f.run)
                && match f.ended_ms {
                    Some(e) => now - e <= IN_FLIGHT_GRACE_MS,
                    None => now - f.started_ms <= IN_FLIGHT_MAX_MS,
                }
        });
        for rs in g.roots.values_mut() {
            rs.tracker.prune(&rl, now);
        }
        let mut released = Vec::new();
        g.claims.retain(|c| {
            if live.contains(&c.run) {
                true
            } else {
                released.push(c.clone());
                false
            }
        });
        let mut expired = Vec::new();
        g.ignores.retain(|i| {
            if i.expires_ms.is_none_or(|e| e > now) {
                true
            } else {
                expired.push(i.id.clone());
                false
            }
        });
        (cleared, released, expired)
    };
    let n = cleared.len();
    for (rec, why) in cleared {
        clear_record(server, rec, why, now);
    }
    for c in released {
        release_claim_store(server, &c, "run_ended");
    }
    if !expired.is_empty() {
        let mut core = server.core.lock().unwrap();
        let mut tx = Tx::new();
        for id in expired {
            tx.m.delete(K_IGNORE, &id);
        }
        let _ = server.commit(&mut core, tx);
    }
    server
        .collision
        .pending_ctx
        .lock()
        .unwrap()
        .retain(|run, v| {
            v.retain(|(at, _)| now - at <= act::CONTEXT_TTL_MS);
            live.contains(run) && !v.is_empty()
        });
    n
}

fn release_claim_store(server: &Server, c: &vc::Claim, reason: &str) {
    let mut core = server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.m.delete(K_CLAIM, &c.id);
    tx.event(
        "task.claim_released",
        json!({"claim": c.id, "run": c.run}),
        json!({"glob": c.glob, "repo": c.root, "reason": reason}),
    );
    let _ = server.commit(&mut core, tx);
}

// ---- JSON -----------------------------------------------------------------------------------

pub(crate) fn collision_json(server: &Server, rec: &vc::CollisionRec, timeline: bool) -> Value {
    let runs: Vec<Value> = server.with_core(|c| {
        rec.runs
            .iter()
            .map(|id| match c.run(id) {
                Some(r) => json!({
                    "run": r.id, "handle": r.handle, "name": r.name, "harness": r.harness,
                    "pane": r.pane, "task": r.task, "state": r.execution.value.as_str(),
                    "alive": run_alive(r),
                }),
                None => json!({
                    "run": id, "handle": null, "name": null, "harness": null,
                    "pane": null, "task": null, "state": null, "alive": false,
                }),
            })
            .collect()
    });
    let mut v = json!({
        "id": rec.id,
        "root": rec.root,
        "severity": rec.severity.as_str(),
        "status": rec.status.as_str(),
        "runs": rec.runs,
        "run_info": runs,
        "paths": rec.paths,
        "ambiguous": rec.ambiguous,
        "first_ms": rec.first_ms,
        "last_ms": rec.last_ms,
        "headline": headline(rec),
        "cleared_ms": rec.cleared_ms,
        "cleared_reason": rec.cleared_reason,
    });
    if timeline {
        v["timeline"] = json!(rec.timeline);
    }
    v
}

pub(crate) fn claim_json(server: &Server, c: &vc::Claim) -> Value {
    let (task, handle) = server.with_core(|core| {
        let r = core.run(&c.run);
        (r.and_then(|r| r.task.clone()), r.map(|r| r.handle.clone()))
    });
    json!({
        "id": c.id, "run": c.run, "run_handle": handle, "root": c.root, "glob": c.glob,
        "created_ms": c.created_ms, "note": c.note, "task": task,
    })
}

fn all_collisions(server: &Server, status: &str, limit: usize) -> Vec<vc::CollisionRec> {
    ensure_loaded(server);
    let mut out: Vec<vc::CollisionRec> = Vec::new();
    if status == "open" || status == "all" {
        out.extend(server.collision.inner.lock().unwrap().open.clone());
    }
    if status != "open" {
        let closed: Vec<vc::CollisionRec> = server.with_core(|c| {
            c.store
                .load_closed(K_COLLISION, MAX_HISTORY)
                .unwrap_or_default()
        });
        out.extend(
            closed
                .into_iter()
                .filter(|r| status == "all" || r.status.as_str() == status),
        );
    }
    out.sort_by_key(|r| std::cmp::Reverse(r.last_ms));
    out.truncate(limit);
    out
}

pub(crate) fn find_collision(server: &Server, id: &str) -> Option<vc::CollisionRec> {
    ensure_loaded(server);
    if let Some(r) = server
        .collision
        .inner
        .lock()
        .unwrap()
        .open
        .iter()
        .find(|r| r.id == id)
        .cloned()
    {
        return Some(r);
    }
    server.with_core(|c| {
        c.store
            .find::<vc::CollisionRec>(K_COLLISION, id)
            .ok()
            .flatten()
    })
}

/// Open collisions of a task (`task.get`): those whose runs include a run of the task.
pub fn for_task(server: &Server, task: &Task) -> Vec<Value> {
    ensure_loaded(server);
    let runs: Vec<String> = server.with_core(|c| {
        c.model
            .runs
            .iter()
            .filter(|r| r.task.as_deref() == Some(task.id.as_str()))
            .map(|r| r.id.clone())
            .collect()
    });
    let recs: Vec<vc::CollisionRec> = server
        .collision
        .inner
        .lock()
        .unwrap()
        .open
        .iter()
        .filter(|r| r.runs.iter().any(|x| runs.contains(x)))
        .cloned()
        .collect();
    recs.iter()
        .map(|r| collision_json(server, r, false))
        .collect()
}

/// The run a pane-scoped caller is: collisions and claims are filtered to it.
fn caller_run(server: &Server, ctx: &Ctx) -> Option<Option<AgentRun>> {
    let pane = ctx.pane_scope.as_ref()?;
    Some(server.with_core(|c| c.run_for_pane(pane).cloned()))
}

// ---- API ------------------------------------------------------------------------------------

pub async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    if !METHODS.iter().any(|(m, _)| *m == method) {
        return None;
    }
    Some(match method {
        "collision.list" => list(server, ctx, p),
        "collision.get" => get(server, ctx, p),
        "collision.status" => Ok(status(server)),
        "collision.ignores" => ignores_list(server, p),
        "collision.ignore" => ignore(server, ctx, p),
        "collision.unignore" => unignore(server, p),
        "collision.pause" => act::pause(server, ctx, p).await,
        "collision.tell" => act::tell(server, ctx, p).await,
        "collision.start_task" => act::start_task(server, ctx, p).await,
        "task.claim" => claim_add(server, ctx, p),
        "task.claims" => claims_list(server, ctx, p),
        "task.claim_release" => claim_release(server, ctx, p),
        _ => return None,
    })
}

fn task_runs(server: &Server, target: &str) -> Result<Vec<String>, vk_proto::rpc::RpcError> {
    server.with_core(|c| {
        let t = c.task(target).ok_or_else(|| not_found("task", target))?;
        Ok(c.model
            .runs
            .iter()
            .filter(|r| r.task.as_deref() == Some(t.id.as_str()))
            .map(|r| r.id.clone())
            .collect())
    })
}

fn list(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let status = s(p, "status").unwrap_or("open");
    if !matches!(status, "open" | "cleared" | "ignored" | "all") {
        return Err(invalid("status: open | cleared | ignored | all"));
    }
    let limit = crate::api::u(p, "limit").unwrap_or(100).clamp(1, 500) as usize;
    let mut recs = all_collisions(server, status, MAX_HISTORY.max(limit));
    if let Some(t) = s(p, "task") {
        let runs = task_runs(server, t)?;
        recs.retain(|r| r.runs.iter().any(|x| runs.contains(x)));
    }
    if let Some(rt) = s(p, "run") {
        let id = run_by_id(server, rt)
            .map(|r| r.id)
            .ok_or_else(|| not_found("run", rt))?;
        recs.retain(|r| r.runs.contains(&id));
    }
    // An agent sees the collisions of its own run only.
    if let Some(me) = caller_run(server, ctx) {
        let id = me.map(|r| r.id).unwrap_or_default();
        recs.retain(|r| r.runs.contains(&id));
    }
    recs.truncate(limit);
    let enabled = config(server).enabled;
    Ok(json!({
        "collisions": recs.iter().map(|r| collision_json(server, r, false)).collect::<Vec<_>>(),
        "enabled": enabled,
    }))
}

fn get(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let id = req(p, "collision")?;
    let rec = find_collision(server, id).ok_or_else(|| not_found("collision", id))?;
    if let Some(me) = caller_run(server, ctx) {
        let mine = me.map(|r| r.id).unwrap_or_default();
        if !rec.runs.contains(&mine) {
            return Err(not_found("collision", id));
        }
    }
    let in_root: Vec<vc::Claim> = server
        .collision
        .inner
        .lock()
        .unwrap()
        .claims
        .iter()
        .filter(|c| c.root == rec.root)
        .cloned()
        .collect();
    let claims: Vec<Value> = in_root.iter().map(|c| claim_json(server, c)).collect();
    let steer = act::steer_report(server, &rec);
    Ok(json!({
        "collision": collision_json(server, &rec, true),
        "claims": claims,
        "steer": steer,
    }))
}

fn status(server: &Arc<Server>) -> Value {
    ensure_loaded(server);
    let cfg = config(server);
    let roots = watch::roots_status(server);
    let (claims, ignores) = {
        let g = server.collision.inner.lock().unwrap();
        (g.claims.len(), g.ignores.len())
    };
    let pending: usize = server
        .collision
        .pending_ctx
        .lock()
        .unwrap()
        .values()
        .map(Vec::len)
        .sum();
    json!({
        "enabled": cfg.enabled,
        "fs_attribution": cfg.fs_attribution.as_str(),
        "watcher": cfg.watcher,
        "window_ms": cfg.window.as_millis() as i64,
        "read_window_ms": cfg.read_window.as_millis() as i64,
        "poll_interval_ms": cfg.poll_interval.as_millis() as i64,
        "enforce_claims": cfg.enforce_claims,
        "roots": roots,
        "claims": claims,
        "ignores": ignores,
        "pending_context": pending,
        "note": "Collision detection is advisory: it warns, and never blocks, reverts or reassigns changes. A watcher and a git poll run only for a checkout that two or more live runs share (or that holds a claim).",
    })
}

fn ignore_json(i: &Ignore) -> Value {
    json!({"id": i.id, "root": i.root, "path": i.path, "created_ms": i.created_ms, "expires_ms": i.expires_ms, "by": i.by})
}

fn ignores_list(server: &Arc<Server>, p: &Value) -> R {
    ensure_loaded(server);
    let g = server.collision.inner.lock().unwrap();
    let root = s(p, "root");
    Ok(json!({
        "ignores": g.ignores.iter().filter(|i| root.is_none_or(|r| i.root == r)).map(ignore_json).collect::<Vec<_>>(),
    }))
}

fn ignore(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    ensure_loaded(server);
    let path = req(p, "path")?;
    let rec = match s(p, "collision") {
        Some(id) => Some(find_collision(server, id).ok_or_else(|| not_found("collision", id))?),
        None => None,
    };
    let root = match (&rec, s(p, "root")) {
        (Some(r), _) => r.root.clone(),
        (None, Some(r)) => r.to_string(),
        (None, None) => return Err(invalid("give `collision` or `root`")),
    };
    let rel = vc::relativize(Path::new(&root), path)
        .or_else(|| vc::normalize(path).filter(|n| !n.is_empty()))
        .ok_or_else(|| invalid("path: outside the repository root"))?;
    let now = now_ms();
    let ig = Ignore {
        id: new_id("ign"),
        root: root.clone(),
        path: rel.clone(),
        created_ms: now,
        expires_ms: crate::api::u(p, "for_secs").map(|s| now + (s as i64).saturating_mul(1000)),
        by: ctx.client_id.clone(),
    };
    let updated = {
        let mut g = server.collision.inner.lock().unwrap();
        g.ignores.push(ig.clone());
        if let Some(rs) = g.roots.get_mut(&root) {
            rs.tracker.forget_path(&rel);
        }
        // Every open record of this root drops the path it names.
        let mut touched = Vec::new();
        for r in g.open.iter_mut().filter(|r| r.root == root) {
            let hit: Vec<String> = r
                .paths
                .iter()
                .filter(|h| vc::glob_match(&rel, &h.path))
                .map(|h| h.path.clone())
                .collect();
            let mut any = false;
            for h in hit {
                any |= r.ignore_path(&h);
            }
            if any {
                touched.push(r.clone());
            }
        }
        g.open.retain(|r| r.status == vc::Status::Open);
        touched
    };
    {
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.m.put(K_IGNORE, &ig.id, None, &ig);
        tx.event(
            "task.collision_action",
            json!({"collision": rec.as_ref().map(|r| r.id.clone())}),
            json!({"action": "ignore", "path": rel, "repo": root, "ignore": ig.id, "by": ctx.client_id}),
        );
        let _ = server.commit(&mut c, tx);
    }
    let mut out_rec = None;
    for r in updated {
        if r.status == vc::Status::Ignored {
            clear_record(server, r.clone(), "ignored", now);
        } else {
            persist(server, &r, None);
        }
        if rec.as_ref().is_some_and(|x| x.id == r.id) {
            out_rec = Some(r);
        }
    }
    Ok(json!({
        "ignore": ignore_json(&ig),
        "collision": out_rec.map(|r| collision_json(server, &r, false)),
    }))
}

fn unignore(server: &Arc<Server>, p: &Value) -> R {
    ensure_loaded(server);
    let id = req(p, "ignore")?;
    let removed = {
        let mut g = server.collision.inner.lock().unwrap();
        let n = g.ignores.len();
        g.ignores.retain(|i| i.id != id);
        g.ignores.len() != n
    };
    if removed {
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.m.delete(K_IGNORE, id);
        let _ = server.commit(&mut c, tx);
    }
    Ok(json!({"removed": removed}))
}

// ---- claims ---------------------------------------------------------------------------------

fn resolve_claim_run(
    server: &Server,
    ctx: &Ctx,
    p: &Value,
) -> Result<AgentRun, vk_proto::rpc::RpcError> {
    let run = if let Some(t) = s(p, "run") {
        run_by_id(server, t).ok_or_else(|| not_found("run", t))?
    } else if let Some(me) = caller_run(server, ctx) {
        me.ok_or_else(|| invalid("this pane has no agent run to claim for"))?
    } else if let Some(t) = s(p, "task") {
        let ids = task_runs(server, t)?;
        let live: Vec<AgentRun> = ids
            .iter()
            .filter_map(|i| run_by_id(server, i))
            .filter(run_alive)
            .collect();
        match live.len() {
            1 => live.into_iter().next().unwrap(),
            0 => return Err(invalid("the task has no live run")),
            _ => return Err(invalid("the task has several runs: name one with `run`")),
        }
    } else {
        return Err(invalid("give `run` (or `task` with one live run)"));
    };
    // An agent manages its own claims only.
    if let Some(pane) = &ctx.pane_scope
        && &run.pane != pane
    {
        return Err(err(
            ErrorKind::PermissionDenied,
            "an agent may only claim for its own run",
        )
        .details(json!({"scope": "pane"})));
    }
    Ok(run)
}

fn claim_add(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    ensure_loaded(server);
    let run = resolve_claim_run(server, ctx, p)?;
    let glob_in = req(p, "glob")?;
    let root = match s(p, "root") {
        Some(r) => vc::repo_root_of(Path::new(r))
            .to_string_lossy()
            .into_owned(),
        None => root_of(server, &run)
            .ok_or_else(|| invalid("the run has no known working directory: pass `root`"))?,
    };
    // An absolute path inside the root is made relative.
    let glob = vc::relativize(Path::new(&root), glob_in)
        .filter(|_| Path::new(glob_in).is_absolute())
        .map(Ok)
        .unwrap_or_else(|| vc::valid_pattern(glob_in).map_err(invalid))?;
    let now = now_ms();
    let (claim, conflicts, created) = {
        let mut g = server.collision.inner.lock().unwrap();
        if let Some(existing) = g
            .claims
            .iter()
            .find(|c| c.run == run.id && c.root == root && c.glob == glob)
            .cloned()
        {
            (existing, vec![], false)
        } else {
            if g.claims.iter().filter(|c| c.run == run.id).count() >= MAX_CLAIMS_PER_RUN
                || g.claims.len() >= MAX_CLAIMS
            {
                return Err(err(ErrorKind::Conflict, "too many claims; release some"));
            }
            let claim = vc::Claim {
                id: new_id("clm"),
                run: run.id.clone(),
                root: root.clone(),
                glob: glob.clone(),
                created_ms: now,
                note: s(p, "note").map(|n| n.chars().take(200).collect()),
            };
            let conflicts: Vec<vc::Claim> = g
                .claims
                .iter()
                .filter(|c| {
                    c.root == root
                        && c.run != run.id
                        && (vc::glob_match(&c.glob, &glob) || vc::glob_match(&glob, &c.glob))
                })
                .cloned()
                .collect();
            g.claims.push(claim.clone());
            (claim, conflicts, true)
        }
    };
    if created {
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.m.put(K_CLAIM, &claim.id, None, &claim);
        tx.event(
            "task.claim_added",
            json!({"claim": claim.id, "run": claim.run}),
            json!({"glob": claim.glob, "repo": claim.root, "note": claim.note, "conflicts": conflicts.iter().map(|c| c.id.clone()).collect::<Vec<_>>(), "by": ctx.client_id}),
        );
        server.commit(&mut c, tx).map_err(internal)?;
    }
    let label = format!(
        "{} claims {} (advisory{})",
        run_label(&run),
        claim.glob,
        if config(server).enforce_claims {
            ", enforced for cooperating adapters"
        } else {
            ""
        }
    );
    Ok(json!({
        "claim": claim_json(server, &claim),
        "conflicts": conflicts.iter().map(|c| claim_json(server, c)).collect::<Vec<_>>(),
        "label": label,
    }))
}

fn claims_list(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    ensure_loaded(server);
    let mut claims: Vec<vc::Claim> = server.collision.inner.lock().unwrap().claims.clone();
    if let Some(t) = s(p, "task") {
        let runs = task_runs(server, t)?;
        claims.retain(|c| runs.contains(&c.run));
    }
    if let Some(rt) = s(p, "run") {
        let id = run_by_id(server, rt)
            .map(|r| r.id)
            .ok_or_else(|| not_found("run", rt))?;
        claims.retain(|c| c.run == id);
    }
    if let Some(r) = s(p, "root") {
        let root = vc::repo_root_of(Path::new(r))
            .to_string_lossy()
            .into_owned();
        claims.retain(|c| c.root == root);
    }
    // An agent sees the claims of its own checkout.
    if let Some(me) = caller_run(server, ctx) {
        let root = me.and_then(|r| root_of(server, &r)).unwrap_or_default();
        claims.retain(|c| c.root == root);
    }
    Ok(json!({"claims": claims.iter().map(|c| claim_json(server, c)).collect::<Vec<_>>()}))
}

fn claim_release(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    ensure_loaded(server);
    if s(p, "claim").is_none() && s(p, "run").is_none() && s(p, "glob").is_none() {
        return Err(invalid("give `claim`, or `run` (optionally with `glob`)"));
    }
    let run_id = match s(p, "run") {
        Some(t) => Some(
            run_by_id(server, t)
                .map(|r| r.id)
                .ok_or_else(|| not_found("run", t))?,
        ),
        None => None,
    };
    let me = caller_run(server, ctx).map(|r| r.map(|r| r.id).unwrap_or_default());
    let released: Vec<vc::Claim> = {
        let mut g = server.collision.inner.lock().unwrap();
        let mut out = Vec::new();
        g.claims.retain(|c| {
            let hit = s(p, "claim").is_none_or(|id| c.id == id)
                && run_id.as_ref().is_none_or(|r| &c.run == r)
                && s(p, "glob").is_none_or(|g| c.glob == g)
                // An agent releases its own claims only.
                && me.as_ref().is_none_or(|m| &c.run == m);
            if hit {
                out.push(c.clone());
            }
            !hit
        });
        out
    };
    for c in &released {
        release_claim_store(server, c, "released");
    }
    Ok(json!({"released": released.iter().map(|c| c.id.clone()).collect::<Vec<_>>()}))
}

// ---- hooks the harness waits on -------------------------------------------------------------

/// What `adapter.signal` returns besides `{}`: a `hook_output` the `vibeke hook` shim prints for
/// the harness (Claude `additionalContext` for a queued steering message, `permissionDecision:
/// deny` for an enforced claim). Called after the signal was routed.
pub fn signal_reply(server: &Arc<Server>, pane: &str, h: Harness, event: &str, p: &Value) -> Value {
    if h.family() != Family::Claude {
        return json!({});
    }
    match act::hook_output(server, pane, event, p) {
        Some(out) => json!({"hook_output": out}),
        None => json!({}),
    }
}

#[cfg(test)]
#[path = "collision_tests.rs"]
mod tests;
