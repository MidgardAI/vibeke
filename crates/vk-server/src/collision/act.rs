//! What the user (or a cooperating adapter) can do about a collision (05 §10 UX): pause one run,
//! tell the runs, start a fresh task from the shared checkout, and the claim guardrail.
//!
//! **Tell** reaches a run only through a native channel: a headless run's own steer/follow-up
//! (pi/omp `steer`, Codex `turn/steer`, Claude stream-json), or Claude's hook `additionalContext`
//! carried by the next `UserPromptSubmit`/`PostToolUse`. Nothing is ever typed into a TUI
//! mid-turn: a run with no such channel is reported `unsupported` with the reason.
//!
//! **Start a fresh task from here** creates a task from the shared checkout's `HEAD` and starts a
//! new run there with a hand-off prompt. The original runs keep working where they are; the
//! shared checkout is not touched (a `git worktree add` only writes worktree metadata).

use super::{Ctx, find_collision, root_of, run_alive, run_by_id, run_label};
use crate::Server;
use crate::agents::harness::{Family, Harness};
use crate::agents::headless;
use crate::api::{R, err, invalid, not_found, req, s};
use crate::core::Tx;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use vk_proto::model::AgentRun;
use vk_proto::rpc::ErrorKind;
use vk_store::now_ms;
use vk_tasks::collision as vc;

/// Queued steering text older than this is dropped undelivered.
pub(crate) const CONTEXT_TTL_MS: i64 = 600_000;
/// Longest steering text.
const TEXT_MAX: usize = 1000;
/// Longest path shown in generated text.
const PATH_MAX: usize = 160;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Channel {
    /// The headless adapter's native steer (or follow-up).
    HeadlessSteer,
    /// Claude hook `additionalContext` on the next prompt or tool result.
    HookContext,
}

impl Channel {
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::HeadlessSteer => "headless_steer",
            Channel::HookContext => "hook_context",
        }
    }
}

/// How a run can be told something, or why it cannot.
pub(crate) fn steer_channel(run: &AgentRun) -> Result<Channel, &'static str> {
    if !run_alive(run) {
        return Err("the run has ended");
    }
    if headless::is_headless(run) {
        if run.integration == "headless:acp" {
            return Err("ACP has no steering");
        }
        return Ok(Channel::HeadlessSteer);
    }
    match Harness::from_id(&run.harness).map(|h| h.family()) {
        Some(Family::Claude) if run.integration == "hooks" => Ok(Channel::HookContext),
        Some(Family::Pi | Family::Omp) => {
            Err("pi/omp steer natively only in headless mode; nothing is typed into a TUI mid-turn")
        }
        _ => Err(
            "this harness has no native steer or follow-up channel; nothing is typed into a TUI mid-turn",
        ),
    }
}

/// Per run of a collision: the channel or the reason there is none.
pub(super) fn steer_report(server: &Server, rec: &vc::CollisionRec) -> Vec<Value> {
    rec.runs
        .iter()
        .map(|id| match run_by_id(server, id) {
            None => json!({"run": id, "channel": null, "reason": "the run has ended"}),
            Some(r) => match steer_channel(&r) {
                Ok(c) => json!({"run": id, "channel": c.as_str()}),
                Err(why) => json!({"run": id, "channel": null, "reason": why}),
            },
        })
        .collect()
}

fn clean(text: &str, max: usize) -> String {
    let t: String = text
        .chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .collect();
    let t = t.trim();
    if t.chars().count() <= max {
        t.to_string()
    } else {
        let mut s: String = t.chars().take(max.saturating_sub(1)).collect();
        s.push('…');
        s
    }
}

fn short_path(p: &str) -> String {
    clean(&p.replace('\n', " "), PATH_MAX)
}

fn pane_handle(server: &Server, r: &AgentRun) -> String {
    server.with_core(|c| {
        c.pane(&r.pane)
            .map(|p| p.handle.clone())
            .unwrap_or_else(|| r.pane.clone())
    })
}

/// "Note: another agent (codex, pane w5:p3) is also editing src/auth.ts — coordinate or avoid."
fn default_text(server: &Server, rec: &vc::CollisionRec, target: &AgentRun) -> String {
    let others: Vec<AgentRun> = rec
        .runs
        .iter()
        .filter(|id| **id != target.id)
        .filter_map(|id| run_by_id(server, id))
        .collect();
    let who: Vec<String> = others
        .iter()
        .map(|r| format!("{}, pane {}", run_label(r), pane_handle(server, r)))
        .collect();
    let paths: Vec<String> = rec
        .headline_paths(3)
        .iter()
        .map(|p| short_path(p))
        .collect();
    let (subject, verb) = match others.len() {
        0 => ("another agent".to_string(), "is"),
        1 => (format!("another agent ({})", who[0]), "is"),
        _ => (format!("other agents ({})", who.join("; ")), "are"),
    };
    format!(
        "Note: {subject} {verb} also editing {} — coordinate or avoid. (Vibeke collision notice; advisory.)",
        paths.join(", ")
    )
}

fn action_event(server: &Server, collision: Option<&str>, data: Value) {
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.event(
        "task.collision_action",
        json!({"collision": collision}),
        data,
    );
    let _ = server.commit(&mut c, tx);
}

// ---- pause ----------------------------------------------------------------------------------

pub(super) async fn pause(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let id = req(p, "collision")?;
    let rec = find_collision(server, id).ok_or_else(|| not_found("collision", id))?;
    let target = req(p, "run")?;
    let run = run_by_id(server, target).ok_or_else(|| not_found("run", target))?;
    if !rec.runs.contains(&run.id) {
        return Err(invalid("that run is not part of this collision"));
    }
    if !run_alive(&run) {
        return Err(err(ErrorKind::Conflict, "the run has ended"));
    }
    // The adapter's own interrupt (agent.interrupt): Escape for a TUI, the protocol's interrupt
    // for a headless run. Nothing else is touched.
    let res = Box::pin(crate::api::dispatch(
        server,
        ctx,
        "agent.interrupt",
        &json!({"target": run.id}),
    ))
    .await?;
    action_event(
        server,
        Some(&rec.id),
        json!({"action": "pause", "run": run.id, "by": ctx.client_id}),
    );
    Ok(json!({"run": res.get("run").cloned().unwrap_or(Value::Null), "paused": true}))
}

// ---- tell -----------------------------------------------------------------------------------

pub(super) async fn tell(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let id = req(p, "collision")?;
    let rec = find_collision(server, id).ok_or_else(|| not_found("collision", id))?;
    let mut targets: Vec<AgentRun> = Vec::new();
    match p.get("runs").and_then(Value::as_array) {
        Some(list) => {
            for v in list {
                let t = v.as_str().ok_or_else(|| invalid("runs: strings"))?;
                let r = run_by_id(server, t).ok_or_else(|| not_found("run", t))?;
                if !rec.runs.contains(&r.id) {
                    return Err(invalid(format!("run {t} is not part of this collision")));
                }
                targets.push(r);
            }
        }
        None => {
            for rid in &rec.runs {
                if let Some(r) = run_by_id(server, rid) {
                    targets.push(r);
                }
            }
        }
    }
    if targets.is_empty() {
        return Err(invalid("no run to tell"));
    }
    let custom = s(p, "text")
        .map(|t| clean(t, TEXT_MAX))
        .filter(|t| !t.is_empty());
    let mut results = Vec::new();
    let mut sample = String::new();
    for run in &targets {
        let text = custom
            .clone()
            .unwrap_or_else(|| default_text(server, &rec, run));
        if sample.is_empty() {
            sample = text.clone();
        }
        match steer_channel(run) {
            Err(why) => results.push(
                json!({"run": run.id, "status": "unsupported", "channel": null, "reason": why}),
            ),
            Ok(Channel::HeadlessSteer) => {
                let r = Box::pin(crate::api::dispatch(
                    server,
                    ctx,
                    "agent.prompt",
                    &json!({"target": run.id, "text": text, "mode": "steer"}),
                ))
                .await;
                match r {
                    Ok(_) => results.push(json!({"run": run.id, "status": "delivered", "channel": "headless_steer"})),
                    Err(e) => results.push(json!({"run": run.id, "status": "failed", "channel": "headless_steer", "reason": e.message})),
                }
            }
            Ok(Channel::HookContext) => {
                server
                    .collision
                    .pending_ctx
                    .lock()
                    .unwrap()
                    .entry(run.id.clone())
                    .or_default()
                    .push((now_ms(), text));
                results.push(json!({"run": run.id, "status": "queued", "channel": "hook_context"}));
            }
        }
    }
    action_event(
        server,
        Some(&rec.id),
        json!({"action": "tell", "results": results, "by": ctx.client_id, "text": clean(&sample, 500)}),
    );
    Ok(json!({"results": results, "text": sample}))
}

/// Context a hook of `pane`'s run should carry back to the harness now (`signal_reply`).
pub(super) fn hook_output(
    server: &Arc<Server>,
    pane: &str,
    event: &str,
    p: &Value,
) -> Option<Value> {
    let run = server.with_core(|c| c.run_for_pane(pane).cloned())?;
    match event {
        "UserPromptSubmit" | "PostToolUse" => {
            let now = now_ms();
            let texts: Vec<String> = {
                let mut q = server.collision.pending_ctx.lock().unwrap();
                let v = q.remove(&run.id)?;
                v.into_iter()
                    .filter(|(at, _)| now - at <= CONTEXT_TTL_MS)
                    .map(|(_, t)| t)
                    .collect()
            };
            if texts.is_empty() {
                return None;
            }
            action_event(
                server,
                None,
                json!({"action": "tell_delivered", "run": run.id, "via": event, "messages": texts.len()}),
            );
            Some(json!({"hookSpecificOutput": {
                "hookEventName": event,
                "additionalContext": texts.join("\n"),
            }}))
        }
        "PreToolUse" => claim_denial(server, &run, p),
        _ => None,
    }
}

/// `collision.enforce_claims`: a *reported* edit tool inside another live run's claim is denied.
/// A courtesy guardrail: shell commands and non-integrated harnesses are unaffected.
pub(super) fn claim_denial(server: &Arc<Server>, run: &AgentRun, p: &Value) -> Option<Value> {
    let cfg = super::config(server);
    if !cfg.enabled || !cfg.enforce_claims {
        return None;
    }
    let tool = p.get("tool_name").and_then(Value::as_str)?;
    if !vc::is_edit_tool(tool) {
        return None;
    }
    let input = p.get("tool_input").cloned().unwrap_or(Value::Null);
    let edits = vc::edit_paths(tool, &input);
    if edits.is_empty() {
        return None;
    }
    super::ensure_loaded(server);
    let root = root_of(server, run)?;
    let claims = super::effective_claims(server, Some(&root));
    for (path, _) in edits {
        let Some(rel) = vc::relativize(Path::new(&root), &path) else {
            continue;
        };
        let hit = claims.iter().find(|c| {
            c.root == root
                && c.run != run.id
                && c.covers(&rel)
                && run_by_id(server, &c.run).is_some_and(|o| run_alive(&o))
        });
        if let Some(c) = hit {
            let owner = run_by_id(server, &c.run)
                .map(|o| run_label(&o))
                .unwrap_or_else(|| c.run.clone());
            action_event(
                server,
                None,
                json!({"action": "claim_denied", "run": run.id, "path": rel, "claim": c.id, "owner": c.run}),
            );
            let reason = format!(
                "{} is inside {owner}'s advisory claim on {} (Vibeke). Coordinate with that agent or pick other files; the user can release the claim with `vibeke claim rm {}`.",
                short_path(&rel),
                short_path(&c.glob),
                c.id
            );
            return Some(json!({"hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason,
            }}));
        }
    }
    None
}

// ---- start a fresh task ---------------------------------------------------------------------

fn git_out(root: &str, args: &[&str]) -> Option<String> {
    let safety = vk_tasks::safety_args(Path::new(root)).ok()?;
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(&safety)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

fn handoff_prompt(
    root: &str,
    branch: Option<&str>,
    head: &str,
    paths: &[String],
    source: Option<&AgentRun>,
) -> String {
    let name = Path::new(root)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.to_string());
    let mut t = String::new();
    t.push_str("This task was split off from a shared checkout.\n\n");
    t.push_str(&format!(
        "Several agents were editing the same checkout ({name}{}) at once and collided on:\n",
        branch.map(|b| format!(", branch {b}")).unwrap_or_default()
    ));
    for p in paths {
        t.push_str(&format!("- {}\n", short_path(p)));
    }
    t.push_str(&format!(
        "\nYou are in a fresh isolated checkout created from that checkout's HEAD ({}). The other agents keep working in the shared checkout and nothing was moved: their uncommitted changes are not here, and yours will not appear there.\n",
        &head[..head.len().min(12)]
    ));
    if let Some(r) = source {
        t.push_str(&format!(
            "\nYou continue the part of the work that {} was doing.",
            run_label(r)
        ));
        if let Some(m) = r.last_message.as_deref().filter(|m| !m.trim().is_empty()) {
            t.push_str(&format!(
                " Its last message was:\n> {}\n",
                crate::items::summarize(m, 600)
            ));
        } else {
            t.push('\n');
        }
    }
    t.push_str("\nTake over the work that touches the files above. Read `git log` and the files before you start, do not assume anything the other agents have not committed, and keep your changes inside this checkout.");
    t
}

pub(super) async fn start_task(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let id = req(p, "collision")?;
    let rec = find_collision(server, id).ok_or_else(|| not_found("collision", id))?;
    let source: Option<AgentRun> = match s(p, "run") {
        Some(t) => {
            let r = run_by_id(server, t).ok_or_else(|| not_found("run", t))?;
            if !rec.runs.contains(&r.id) {
                return Err(invalid("that run is not part of this collision"));
            }
            Some(r)
        }
        None => rec
            .runs
            .iter()
            .filter_map(|i| run_by_id(server, i))
            .find(run_alive),
    };
    let root = rec.root.clone();
    let (r2, r3) = (root.clone(), root.clone());
    let head = tokio::task::spawn_blocking(move || git_out(&r2, &["rev-parse", "HEAD"]))
        .await
        .map_err(crate::api::internal)?
        .ok_or_else(|| {
            invalid("the shared checkout is not a Git repository with a commit to start from")
        })?;
    let branch = tokio::task::spawn_blocking(move || {
        git_out(&r3, &["rev-parse", "--abbrev-ref", "HEAD"]).filter(|b| b != "HEAD")
    })
    .await
    .map_err(crate::api::internal)?;
    let paths = rec.headline_paths(8);
    let harness = s(p, "harness")
        .map(str::to_string)
        .or_else(|| source.as_ref().map(|r| r.harness.clone()))
        .unwrap_or_else(|| "claude".into());
    let title = s(p, "title")
        .map(|t| clean(t, 80))
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| {
            clean(
                &format!(
                    "Split from shared checkout: {}",
                    paths
                        .first()
                        .map(|p| short_path(p))
                        .unwrap_or_else(|| "shared work".into())
                ),
                80,
            )
        });
    let prompt = s(p, "prompt")
        .map(|t| clean(t, 8000))
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| {
            handoff_prompt(&root, branch.as_deref(), &head, &paths, source.as_ref())
        });
    if p.get("dry_run").and_then(Value::as_bool).unwrap_or(false) {
        return Ok(json!({
            "created": false, "base": head, "title": title, "harness": harness,
            "prompt": prompt, "source_run": source.map(|r| r.id), "result": null,
        }));
    }
    let params = json!({
        "title": title,
        "repo": root,
        "base": head,
        "isolation": "worktree",
        "fetch": false,
        "agents": [{"harness": harness, "prompt": prompt}],
    });
    let res = Box::pin(crate::api::dispatch(server, ctx, "task.create", &params)).await?;
    action_event(
        server,
        Some(&rec.id),
        json!({
            "action": "start_task", "by": ctx.client_id, "base": head,
            "task": res.pointer("/task/id"), "source_run": source.as_ref().map(|r| r.id.clone()),
        }),
    );
    Ok(json!({
        "created": true, "base": head, "title": title, "harness": harness,
        "prompt": prompt, "source_run": source.map(|r| r.id), "result": res,
    }))
}
