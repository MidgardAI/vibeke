//! `task.split` (05 §11): move a running agent's uncommitted changes from a shared checkout
//! into a new task worktree. The git sequence (capture, recovery ref, select, validate, apply,
//! verify, revert) is `vk_orchestrate::split`; this module quiesces the writers, creates the
//! destination task from the source's exact `HEAD`, and resumes or hands off the agent.

use crate::Server;
use crate::api::{Ctx, R, b, err, internal, invalid, not_found, s};
use crate::orch::{self, call, from_orch, require};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use vk_orchestrate::gitx;
use vk_orchestrate::split as sp;
use vk_proto::model::{AgentRun, Execution};
use vk_proto::rpc::ErrorKind;

const SHELLS: &[&str] = &[
    "sh", "bash", "zsh", "fish", "nu", "dash", "ksh", "tcsh", "pwsh",
];

fn source_dir(server: &Server, ctx: &Ctx, p: &Value) -> Result<PathBuf, vk_proto::rpc::RpcError> {
    let cwd: Option<String> = if let Some(r) = s(p, "run") {
        server.with_core(|c| c.run(r).and_then(|r| r.cwd.clone()))
    } else if let Some(pane) = s(p, "pane") {
        let pane = crate::api::resolve_pane(server, ctx, Some(pane))?;
        server.pane_cwd(&pane.id)
    } else if let Some(ws) = s(p, "workspace") {
        let ws = crate::api::resolve_ws(server, ctx, Some(ws))?;
        server.with_core(|c| {
            c.model
                .panes
                .iter()
                .find(|x| x.workspace == ws.id)
                .and_then(|x| x.cwd.clone())
        })
    } else {
        crate::api::resolve_pane(server, ctx, None)
            .ok()
            .and_then(|x| server.pane_cwd(&x.id))
    };
    let cwd = cwd.ok_or_else(|| {
        invalid(
            "give `run`, `pane` or `workspace` (or call from a pane) to name the checkout to split",
        )
    })?;
    let top = gitx::run(Path::new(&cwd), &["rev-parse", "--show-toplevel"])
        .map_err(|_| invalid(format!("{cwd} is not inside a git checkout")))?;
    Ok(PathBuf::from(top))
}

fn under(dir: &Path, cwd: &str) -> bool {
    Path::new(cwd).starts_with(dir)
}

fn runs_in(server: &Server, root: &Path) -> Vec<AgentRun> {
    server.with_core(|c| {
        c.model
            .runs
            .iter()
            .filter(|r| r.ended_at_ms.is_none())
            .filter(|r| {
                r.cwd.as_deref().is_some_and(|d| under(root, d))
                    || c.pane(&r.pane)
                        .and_then(|p| p.cwd.as_deref())
                        .is_some_and(|d| under(root, d))
            })
            .cloned()
            .collect()
    })
}

/// Panes with a foreground process that is neither a shell nor an agent in the checkout.
fn other_writers(server: &Server, root: &Path, runs: &[AgentRun]) -> Vec<sp::Writer> {
    server.with_core(|c| {
        c.model
            .panes
            .iter()
            .filter(|p| !p.exited && p.cwd.as_deref().is_some_and(|d| under(root, d)))
            .filter(|p| !runs.iter().any(|r| r.pane == p.id))
            .filter_map(|p| {
                let prog = p.fg_cmdline.first()?;
                let base = prog
                    .rsplit('/')
                    .next()
                    .unwrap_or(prog)
                    .trim_start_matches('-');
                (!SHELLS.contains(&base)).then(|| sp::Writer {
                    label: format!("{} ({base})", p.handle),
                    agent: false,
                    quiet: false,
                })
            })
            .collect()
    })
}

async fn interrupt_and_wait(server: &Arc<Server>, runs: &[AgentRun], wait: Duration) {
    for r in runs {
        let _ = call(server, "agent.interrupt", json!({"target": r.id})).await;
    }
    let until = Instant::now() + wait;
    loop {
        let busy = server.with_core(|c| {
            runs.iter().any(|r| {
                c.run(&r.id).is_some_and(|x| {
                    x.execution.value == Execution::Working
                        || x.execution.value == Execution::Starting
                })
            })
        });
        if !busy || Instant::now() >= until {
            return;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

pub async fn split(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let c = orch::cfg(server);
    require(c.split.enabled, "split into task", "split")?;
    let root = source_dir(server, ctx, p)?;
    let selected: Option<Vec<String>> = p.get("paths").and_then(Value::as_array).map(|a| {
        a.iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect()
    });
    let dry = b(p, "dry_run").unwrap_or(false);
    let runs = runs_in(server, &root);
    let moved_runs: Vec<AgentRun> = match s(p, "run") {
        Some(r) => runs
            .iter()
            .filter(|x| x.id == r || x.handle == r)
            .cloned()
            .collect(),
        None => runs.clone(),
    };
    if s(p, "run").is_some() && moved_runs.is_empty() {
        return Err(not_found("run", s(p, "run").unwrap_or("")));
    }
    let changes = {
        let r = root.clone();
        tokio::task::spawn_blocking(move || sp::changed_paths(&r))
            .await
            .map_err(internal)?
            .map_err(from_orch)?
    };
    if changes.is_empty() {
        return Err(err(
            ErrorKind::Conflict,
            "nothing to split: the checkout has no uncommitted changes",
        ));
    }
    if dry {
        let cap = sp::Captured {
            id: "dry-run".into(),
            head: String::new(),
            branch: None,
            changes: changes.clone(),
            recovery_ref: format!("{}/<id>", sp::RECOVERY_NS),
            recovery_commit: String::new(),
            digest: String::new(),
            captured_at_ms: 0,
        };
        let sel = sp::Selection {
            paths: selected
                .clone()
                .unwrap_or_else(|| changes.iter().map(|c| c.path.clone()).collect()),
            digest: String::new(),
        };
        let writers = {
            let mut w: Vec<sp::Writer> = runs
                .iter()
                .map(|r| sp::Writer {
                    label: r.handle.clone(),
                    agent: true,
                    quiet: r.execution.value != Execution::Working,
                })
                .collect();
            w.extend(other_writers(server, &root, &runs));
            w
        };
        let paths: Vec<String> = changes.iter().map(|c| c.path.clone()).collect();
        let newest = sp::newest_mtime_ms(&root, &paths);
        let blocked = sp::quiesce_check(
            &writers,
            newest,
            vk_store::now_ms(),
            c.split.quiet_for().as_millis() as i64,
        )
        .err();
        return Ok(json!({
            "dry_run": true,
            "source": root,
            "changes": changes,
            "runs": runs.iter().map(|r| json!({"run": r.id, "handle": r.handle, "harness": r.harness, "execution": r.execution.value.as_str()})).collect::<Vec<_>>(),
            "steps": sp::plan_steps(&cap, &sel).into_iter().map(|(n, d)| json!({"step": n, "detail": d})).collect::<Vec<_>>(),
            "blocked_by": blocked,
        }));
    }

    // 1. Quiesce every writer in the checkout (not only the run being moved).
    interrupt_and_wait(server, &runs, Duration::from_secs(15)).await;
    let mut writers: Vec<sp::Writer> = server.with_core(|core| {
        runs.iter()
            .map(|r| {
                let x = core.run(&r.id);
                sp::Writer {
                    label: r.handle.clone(),
                    agent: true,
                    quiet: x.is_none_or(|x| {
                        !matches!(x.execution.value, Execution::Working | Execution::Starting)
                    }),
                }
            })
            .collect()
    });
    writers.extend(other_writers(server, &root, &runs));
    let paths: Vec<String> = changes.iter().map(|c| c.path.clone()).collect();
    let newest = sp::newest_mtime_ms(&root, &paths);
    if let Err(why) = sp::quiesce_check(
        &writers,
        newest,
        vk_store::now_ms(),
        c.split.quiet_for().as_millis() as i64,
    ) {
        return Err(err(
            ErrorKind::Conflict,
            format!("the checkout is not quiet: {}", why.join("; ")),
        )
        .details(json!({"reason": "not_quiet", "why": why})));
    }

    // 2-3. Capture (recovery ref) and select.
    let id = crate::core::ulid().to_lowercase()[16..].to_string();
    let (r2, sel2) = (root.clone(), selected.clone());
    let (cap, sel) = tokio::task::spawn_blocking(move || {
        let cap = sp::capture(&r2, &id)?;
        let sel = sp::select(&r2, &cap, sel2.as_deref())?;
        Ok::<_, vk_orchestrate::Error>((cap, sel))
    })
    .await
    .map_err(internal)?
    .map_err(from_orch)?;

    // 4. The destination: a fresh worktree of exactly the source's HEAD. No setup and no file
    // copies (either would make it unclean and the apply would refuse).
    let title = s(p, "title").map(str::to_string).unwrap_or_else(|| {
        format!(
            "split from {}",
            cap.branch.clone().unwrap_or_else(|| "detached HEAD".into())
        )
    });
    let mut params = json!({"title": title, "repo": root, "base": cap.head, "isolation": "worktree", "setup": false, "copy_files": []});
    if let Some(slug) = s(p, "slug") {
        params["slug"] = json!(slug);
    }
    let created = call(server, "task.create", params).await?;
    let task_id = created["task"]["id"].as_str().unwrap_or("").to_string();
    let dest = created["task"]["worktree_path"].as_str().map(PathBuf::from);
    let Some(dest) = dest else {
        return Err(internal("the new task has no worktree"));
    };
    let abandon = |server: Arc<Server>, task: String| async move {
        let _ = call(
            &server,
            "task.finish",
            json!({"task": task, "remove_worktree": true, "archive": true, "force": true}),
        )
        .await;
    };

    // 5. Validate, apply, verify, revert the source.
    let (r3, d3, c3, s3) = (root.clone(), dest.clone(), cap.clone(), sel.clone());
    let res = tokio::task::spawn_blocking(move || sp::execute(&r3, &d3, &c3, &s3))
        .await
        .map_err(internal)?;
    let result = match res {
        Ok(r) => r,
        Err(e) => {
            abandon(server.clone(), task_id.clone()).await;
            return Err(err(
                ErrorKind::Conflict,
                format!("split failed, nothing was moved: {e}"),
            )
            .details(json!({"recovery_ref": cap.recovery_ref})));
        }
    };

    // 6. Resume or hand off the moved agents in the new worktree.
    let pane = created["panes"][0]["id"].as_str().unwrap_or("").to_string();
    let branch = created["task"]["branch"].as_str().unwrap_or("").to_string();
    let mut started = vec![];
    let mut resumed_all = true;
    for r in &moved_runs {
        let want_resume = b(p, "resume").unwrap_or(c.split.resume);
        if want_resume {
            match call(server, "agent.resume", json!({"run": r.id, "pane": pane})).await {
                Ok(v) => {
                    started.push(json!({"from": r.handle, "via": "resume", "run": v}));
                    continue;
                }
                Err(e) => started.push(json!({"from": r.handle, "resume_failed": e.message})),
            }
        }
        let last = r.last_message.clone().unwrap_or_default();
        let last: String = last.chars().take(600).collect();
        let prompt = format!(
            "Your uncommitted work was moved out of a shared checkout into this new git worktree (task {}, branch {}). \
{} path(s) were carried over, staged and unstaged state preserved. Continue the work you were doing here and do not touch the original checkout.{}",
            created["task"]["handle"].as_str().unwrap_or(""),
            branch,
            result.moved.len(),
            if last.is_empty() {
                String::new()
            } else {
                format!("\n\nYour last message was:\n{last}")
            }
        );
        let opts = crate::sandbox::LaunchOpts::default();
        match crate::agents::start_in_pane_opts(
            server,
            &pane,
            &r.harness,
            None,
            Some(&prompt),
            &[],
            Some(&task_id),
            &opts,
        )
        .await
        {
            Ok(v) => started.push(json!({"from": r.handle, "via": "hand_off", "run": v})),
            Err(e) => {
                resumed_all = false;
                started.push(json!({"from": r.handle, "error": e.message}));
            }
        }
    }
    if resumed_all && !b(p, "keep_recovery").unwrap_or(false) {
        let (r4, i4) = (root.clone(), cap.id.clone());
        let _ = tokio::task::spawn_blocking(move || sp::drop_recovery(&r4, &i4)).await;
    }
    orch::emit(
        server,
        "task.split",
        json!({"task": task_id}),
        json!({"source": root, "moved": result.moved.len(), "recovery_ref": result.recovery_ref, "recovery_kept": !resumed_all || b(p, "keep_recovery").unwrap_or(false), "runs": started.len()}),
    );
    let task = server.with_core(|core| core.task(&task_id).cloned());
    Ok(json!({
        "task": task,
        "source": root,
        "moved": result.moved,
        "recovery_ref": result.recovery_ref,
        "recovery_kept": !resumed_all || b(p, "keep_recovery").unwrap_or(false),
        "runs": started,
    }))
}
