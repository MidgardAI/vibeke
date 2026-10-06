//! Task workspace features (05): `.vibeke/task.toml` resolution with user overrides and repo
//! trust, the setup run (visible `setup` pane, flags), the reconcile loop and PR status.
//!
//! Trust (09 §4): anything that runs repo-provided code (`setup.script`, `setup.run`,
//! `deps.install` — including the auto-detected package-manager install, whose lifecycle scripts
//! are repo code — and the repo's `[env]`) only takes effect for the exact `.vibeke/` tree the
//! user trusted with `policy.trust`. Until then the `task.setup_untrusted` event carries the
//! commands, so they are shown before anything runs. Commands set in the user's own
//! `[tasks.repos."…"]` config are trusted by definition. The reconcile loop only ever marks
//! tasks and emits events: it never deletes a directory, branch or worktree.

use crate::Server;
use crate::api::{R, err, internal, invalid, not_found, req, s};
use crate::core::Tx;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use vk_proto::model::{Task, TaskOwnership};
use vk_proto::rpc::ErrorKind;
use vk_tasks::{DepsPlan, PlannedCommand, TaskFile, TemplateVars};

/// Default setup timeout when `setup.timeout` is not set.
const DEFAULT_SETUP_TIMEOUT: Duration = Duration::from_secs(600);

/// The `[tasks]` section of the user config (defaults when unreadable).
pub(crate) fn load_tasks_cfg() -> vk_config::Tasks {
    vk_config::Config::load(vk_config::config_path())
        .map(|(c, _)| c.tasks)
        .unwrap_or_default()
}

/// A repo's task configuration after merging the repo file with the user's override.
#[derive(Debug, Clone, Default)]
pub(crate) struct Resolved {
    /// Merged: repo file, then user override on top.
    pub file: TaskFile,
    /// The user's override for this repo (trusted), if any.
    pub user: Option<TaskFile>,
    pub warnings: Vec<String>,
}

fn user_override(
    cfg: &vk_config::Tasks,
    repo_root: &Path,
    remote: Option<&str>,
) -> Option<TaskFile> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    cfg.repos
        .iter()
        .find(|(k, _)| vk_tasks::repo_key_matches(k, repo_root, remote, home.as_deref()))
        .and_then(|(_, o)| {
            serde_json::to_value(o)
                .ok()
                .and_then(|v| serde_json::from_value(v).ok())
        })
}

/// Read `<checkout>/.vibeke/task.toml` (the version in the new worktree: the tree the trust
/// digest covers) and apply the user's override for the repo.
pub(crate) fn resolve(repo_root: &Path, checkout: &Path, cfg: &vk_config::Tasks) -> Resolved {
    let mut warnings = Vec::new();
    let mut file = match TaskFile::load(checkout) {
        Ok(f) => f.unwrap_or_default(),
        Err(e) => {
            warnings.push(format!("ignoring {}: {e}", vk_tasks::TASK_FILE));
            TaskFile::default()
        }
    };
    let remote = vk_tasks::repo_root(repo_root).and_then(|i| i.remote_url);
    let user = user_override(cfg, repo_root, remote.as_deref());
    if let Some(u) = &user {
        file.apply(u);
    }
    warnings.extend(file.warnings());
    Resolved {
        file,
        user,
        warnings,
    }
}

impl Resolved {
    /// Is this command the user's own (from `[tasks.repos]`), so it needs no trust?
    fn user_owns(&self, source: &str, command: &str) -> bool {
        let Some(u) = &self.user else { return false };
        match source {
            "deps.install" => u.deps.install.as_deref() == Some(command),
            "setup.run" => u.setup.run.iter().any(|c| c == command),
            _ => false,
        }
    }
}

/// Everything the setup step will do for one task.
#[derive(Debug, Clone)]
pub(crate) struct SetupPlan {
    /// Rendered shell commands, in order: install, then `setup.run`. Template variables
    /// are references to `template_env` (`"$VIBEKE_BRANCH"`), never pasted values.
    pub commands: Vec<PlannedCommand>,
    /// Values of the template variables, exported to the setup commands (last, so `[env]`
    /// cannot redefine them).
    pub template_env: Vec<(String, String)>,
    /// Repo-relative script, if it exists in the checkout.
    pub script: Option<String>,
    /// Does any step run code the user has not vouched for?
    pub needs_trust: bool,
    pub timeout: Duration,
    pub start_agents_on_failure: bool,
    pub parallel_agent: bool,
    /// Env for setup and every pane: `VIBEKE_TASK_SLUG`, `[ports] env`, and `[env]`
    /// (the repo's `[env]` only when trusted).
    pub env: Vec<(String, String)>,
    /// Is the repo trusted for exactly the `.vibeke/` tree of this checkout?
    pub trusted: bool,
    /// blake3 of that tree (shown with `task.setup_untrusted`).
    pub digest: Option<String>,
}

impl SetupPlan {
    pub fn has_steps(&self) -> bool {
        !self.commands.is_empty() || self.script.is_some()
    }
}

/// Template variables for one task.
pub(crate) fn vars_for(
    task_id: &str,
    slug: &str,
    branch: Option<&str>,
    lease: Option<&vk_tasks::Lease>,
    repo_root: &Path,
    worktree: &Path,
) -> TemplateVars {
    TemplateVars {
        slug: slug.to_string(),
        branch: branch.unwrap_or_default().to_string(),
        task: task_id.to_string(),
        port_base: lease.map(|l| l.start),
        port_end: lease.map(|l| l.end),
        repo_root: repo_root.to_string_lossy().into_owned(),
        worktree: worktree.to_string_lossy().into_owned(),
        source_root: repo_root.to_string_lossy().into_owned(),
    }
}

/// Build the setup plan. `script_param` is the `task.create` param, then the task file's
/// `setup.script`, then `tasks.setup_script`.
pub(crate) fn plan_setup(
    resolved: &Resolved,
    deps: Option<&DepsPlan>,
    vars: &TemplateVars,
    checkout: &Path,
    script_param: Option<&str>,
    cfg_script: &str,
    trusted: bool,
) -> SetupPlan {
    let file = &resolved.file;
    let script_rel = script_param
        .map(str::to_string)
        .or_else(|| file.setup.script.clone())
        .unwrap_or_else(|| cfg_script.to_string());
    let script =
        (!script_rel.is_empty() && checkout.join(&script_rel).is_file()).then_some(script_rel);
    let install = deps.and_then(DepsPlan::install_command).map(str::to_string);
    let raw: Vec<PlannedCommand> = file
        .commands(install.as_deref())
        .into_iter()
        .filter(|c| c.source != "setup.script")
        .collect();
    let all_user_owned = raw.iter().all(|c| resolved.user_owns(c.source, &c.command));
    // Commands reference `$VIBEKE_BRANCH` & co. instead of containing the values (a branch
    // name is not shell code); `template_env` carries the values.
    let commands: Vec<PlannedCommand> = raw
        .iter()
        .map(|c| PlannedCommand {
            source: c.source,
            command: vars.render_shell(&c.command),
        })
        .collect();
    // A script is always repo content; commands are the user's only when they wrote them.
    let needs_trust = script.is_some() || !all_user_owned;
    // Env: the user's own `[env]` always; the repo's only when trusted.
    let mut env_file = file.clone();
    if !trusted && let Some(u) = &resolved.user {
        env_file.env = u.env.clone();
    } else if !trusted {
        env_file.env.clear();
    }
    // `[ports] env`: an untrusted repo may only set `PORT`/`*_PORT` (a name like `ZDOTDIR`
    // would point the pane's shell at a repo directory); shell and loader variables are
    // never settable. Names the user declared in their own override pass.
    let declared = resolved
        .user
        .as_ref()
        .map(|u| u.ports.env.clone())
        .unwrap_or_default();
    env_file.ports.env = vk_tasks::filter_port_env(&env_file.ports.env, &declared, trusted);
    let mut env = vec![("VIBEKE_TASK_SLUG".to_string(), vars.slug.clone())];
    env.extend(vars.env_pairs(&env_file));
    SetupPlan {
        commands,
        template_env: vars.shell_env(),
        script,
        needs_trust,
        timeout: file.setup_timeout().unwrap_or(DEFAULT_SETUP_TIMEOUT),
        start_agents_on_failure: file.setup.start_agents_on_failure.unwrap_or(false),
        parallel_agent: file.setup.parallel_agent.unwrap_or(false),
        env,
        trusted,
        digest: None,
    }
}

/// `{source, command}` JSON for events and the trust prompt.
pub(crate) fn commands_json(plan: &SetupPlan) -> Vec<Value> {
    let mut v: Vec<Value> = plan
        .commands
        .iter()
        .map(|c| json!({"source": c.source, "command": c.command}))
        .collect();
    if let Some(sc) = &plan.script {
        v.push(json!({"source": "setup.script", "command": format!("sh {sc}")}));
    }
    v
}

/// What the setup launch did.
pub(crate) struct SetupLaunch {
    /// Resolves with `true` when setup succeeded (or had nothing to do), `false` when it
    /// failed, timed out or was cancelled. `None` when nothing is running.
    pub done: Option<tokio::sync::oneshot::Receiver<bool>>,
    /// The visible `setup` pane.
    pub pane: Option<String>,
}

/// Start (or refuse, for lack of trust) the setup of a task: marks `setup_status`, opens the
/// `setup` pane showing the live log, runs the steps on a thread. Nothing is awaited here.
pub(crate) fn launch_setup(
    server: &Arc<Server>,
    task_id: &str,
    repo_root: &Path,
    checkout: &Path,
    plan: &SetupPlan,
    lease: Option<vk_tasks::Lease>,
    split_from: &str,
) -> SetupLaunch {
    if !plan.has_steps() {
        return SetupLaunch {
            done: None,
            pane: None,
        };
    }
    if plan.needs_trust && !plan.trusted {
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        if let Some(mut t) = c.task(task_id).cloned() {
            t.setup_status = Some("untrusted".into());
            tx.task(t);
        }
        let script = plan.script.as_deref().unwrap_or("");
        tx.event(
            "task.setup_untrusted",
            json!({"task": task_id}),
            json!({
                "repo": repo_root,
                "digest": plan.digest.as_deref().unwrap_or(""),
                "script": checkout.join(script),
                "commands": commands_json(plan),
                "hint": format!("review the commands, then run: vibeke policy trust {}", repo_root.display()),
            }),
        );
        let _ = server.commit(&mut c, tx);
        return SetupLaunch {
            done: None,
            pane: None,
        };
    }
    let log = checkout.join(".vibeke/setup.log");
    if let Some(d) = log.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let _ = std::fs::File::create(&log);
    let pane = server
        .split_pane(
            split_from,
            vk_proto::layout::Direction::Down,
            0.3,
            Some(&checkout.to_string_lossy()),
            Some(vec![
                "tail".into(),
                "-n".into(),
                "+1".into(),
                "-F".into(),
                log.to_string_lossy().into_owned(),
            ]),
            Some("setup".into()),
            None,
            "task:setup",
        )
        .ok()
        .map(|p| p.id);
    let opts = setup_options(plan, task_id, checkout, lease, log);
    let (tx_done, rx_done) = tokio::sync::oneshot::channel();
    let srv = server.clone();
    let id = task_id.to_string();
    let pane2 = pane.clone();
    let cmds = commands_json(plan);
    std::thread::spawn(move || {
        set_setup(
            &srv,
            &id,
            "running",
            "task.setup_started",
            json!({"commands": cmds, "pane": pane2}),
        );
        let out = vk_tasks::run_setup(&opts, &vk_tasks::CancelToken::default());
        let (status, ok, code, secs) = match &out {
            Ok(o) => {
                let ok = matches!(
                    o.status,
                    vk_tasks::SetupStatus::Succeeded | vk_tasks::SetupStatus::Skipped
                );
                let code = match o.status {
                    vk_tasks::SetupStatus::Failed { exit_code } => exit_code,
                    _ => None,
                };
                (
                    format!("{:?}", o.status),
                    ok,
                    code,
                    o.duration.as_secs_f64(),
                )
            }
            Err(e) => (format!("failed: {e}"), false, None, 0.0),
        };
        let data = json!({"status": status, "exit_code": code, "duration_ms": (secs * 1000.0) as u64, "log": opts.log_path, "pane": pane2});
        set_setup(&srv, &id, &status, "task.setup_finished", data.clone());
        if !ok {
            let mut c = srv.core.lock().unwrap();
            let mut tx = Tx::new();
            tx.event("task.setup_failed", json!({"task": id}), data);
            let _ = srv.commit(&mut c, tx);
            drop(c);
            srv.notify(
                "task",
                pane2.as_deref(),
                "Task setup failed",
                &format!("{status}; see the setup pane"),
                "high",
            );
        }
        let _ = tx_done.send(ok);
    });
    SetupLaunch {
        done: Some(rx_done),
        pane,
    }
}

/// What the host setup runner gets for a plan: the plan's env, then the template variables
/// its commands reference (last, so `[env]` cannot redefine them).
fn setup_options(
    plan: &SetupPlan,
    task_id: &str,
    checkout: &Path,
    lease: Option<vk_tasks::Lease>,
    log: PathBuf,
) -> vk_tasks::SetupOptions {
    vk_tasks::SetupOptions {
        worktree: checkout.to_path_buf(),
        script: plan.script.clone().map(PathBuf::from).unwrap_or_default(),
        commands: plan.commands.iter().map(|c| c.command.clone()).collect(),
        task_id: task_id.to_string(),
        lease,
        extra_env: plan.env.iter().chain(&plan.template_env).cloned().collect(),
        log_path: log,
        timeout: Some(plan.timeout),
    }
}

fn set_setup(server: &Server, task_id: &str, status: &str, event: &str, data: Value) {
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    if let Some(mut t) = c.task(task_id).cloned() {
        t.setup_status = Some(status.to_string());
        tx.task(t);
    }
    tx.event(event, json!({"task": task_id}), data);
    let _ = server.commit(&mut c, tx);
}

// ---- trust display ------------------------------------------------------------------------

/// What `policy.trust` shows for `<repo>/.vibeke/task.toml`: every command it would run and
/// the env names it sets (values are shown too: they are configuration, not secrets).
pub(crate) fn trust_summary(repo: &Path) -> Value {
    match TaskFile::load(repo) {
        Ok(Some(f)) => {
            let install = f.deps.install.clone();
            json!({
                "file": vk_tasks::TASK_FILE,
                "commands": f.commands(install.as_deref()).iter().map(|c| json!({"source": c.source, "command": c.command})).collect::<Vec<_>>(),
                "env": f.env,
                "auto_install": f.deps.strategy.is_some() && install.is_none(),
                "warnings": f.warnings(),
            })
        }
        Ok(None) => Value::Null,
        Err(e) => json!({"file": vk_tasks::TASK_FILE, "error": e.to_string()}),
    }
}

// ---- API: task.setup (rerun), task.pr, task.reconcile ------------------------------------

fn owned_task(server: &Server, t: &str) -> Result<Task, vk_proto::rpc::RpcError> {
    let task = server
        .with_core(|c| c.task(t).cloned())
        .ok_or_else(|| not_found("task", t))?;
    if task.ownership == TaskOwnership::Attached {
        return Err(err(
            ErrorKind::Conflict,
            "an attached task has no setup, worktree or PR of its own",
        ));
    }
    Ok(task)
}

/// `task.setup {task}`: run the task's setup again (after `policy trust`, or after fixing the
/// script). Shows the `setup` pane; agents are not touched.
pub(crate) async fn task_setup(server: &Arc<Server>, p: &Value) -> R {
    let t = req(p, "task")?;
    let task = owned_task(server, t)?;
    // A container task's setup runs in its box, through the container runner; never on the
    // host (where a trusted container script would get host files and network).
    if task.isolation.level == vk_proto::model::IsolationLevel::Container {
        return container_setup(server, &task).await;
    }
    let wt = task
        .worktree_path
        .clone()
        .map(PathBuf::from)
        .ok_or_else(|| invalid("the task has no checkout"))?;
    let repo = PathBuf::from(&task.repo_root);
    let cfg = load_tasks_cfg();
    let resolved = resolve(&repo, &wt, &cfg);
    let deps = (resolved.file.deps.strategy.is_some() || resolved.file.deps.install.is_some())
        .then(|| vk_tasks::plan_deps(&resolved.file.deps, &repo, &wt));
    let lease = task.port_range.map(|(start, end)| vk_tasks::Lease {
        start,
        end,
        task_id: task.id.clone(),
        session: server.opts.session.clone(),
        owner_pid: None,
        created_at: 0,
    });
    let digest = crate::run::vibeke_dir_digest(&wt);
    let trusted = digest
        .as_ref()
        .is_some_and(|d| crate::run::repo_trusted(server, &repo, d));
    let vars = vars_for(
        &task.id,
        &task.slug,
        task.branch.as_deref(),
        lease.as_ref(),
        &repo,
        &wt,
    );
    let mut plan = plan_setup(
        &resolved,
        deps.as_ref(),
        &vars,
        &wt,
        s(p, "setup_script"),
        &cfg.setup_script,
        trusted,
    );
    plan.digest = digest.clone();
    let split_from = server
        .with_core(|c| {
            c.model
                .panes
                .iter()
                .find(|x| Some(&x.workspace) == task.workspace.as_ref())
                .map(|x| x.id.clone())
        })
        .ok_or_else(|| err(ErrorKind::Conflict, "the task's workspace has no pane"))?;
    let launch = launch_setup(server, &task.id, &repo, &wt, &plan, lease, &split_from);
    let status = server.with_core(|c| c.task(&task.id).and_then(|t| t.setup_status.clone()));
    Ok(json!({
        "task": task.id,
        "started": launch.done.is_some(),
        "pane": launch.pane,
        "setup_status": status,
        "commands": commands_json(&plan),
        "needs_trust": plan.needs_trust && !trusted,
    }))
}

/// `task.setup` for a container task: refuse unless its box exists and is a container box.
async fn container_setup(server: &Arc<Server>, task: &Task) -> R {
    let tb = server.sandbox.get(&task.id).ok_or_else(|| {
        err(
            ErrorKind::Conflict,
            "the task's container box is gone; setup is not run on the host (start the box with `vibeke sandbox start`)",
        )
    })?;
    if !matches!(tb.runner, crate::sandbox::BoxRunner::Container(_)) {
        return Err(err(
            ErrorKind::Conflict,
            "the task is container-isolated but its box is not a container; refusing to run setup",
        ));
    }
    let srv = server.clone();
    let mut out = tokio::task::spawn_blocking(move || {
        let crate::sandbox::BoxRunner::Container(c) = &tb.runner else {
            unreachable!()
        };
        crate::sandbox::container::rerun_setup(&srv, &tb, c)
    })
    .await
    .map_err(internal)??;
    out["task"] = json!(task.id);
    out["in_container"] = json!(true);
    Ok(out)
}

fn pr_cache() -> &'static vk_tasks::PrCache {
    static C: OnceLock<vk_tasks::PrCache> = OnceLock::new();
    C.get_or_init(vk_tasks::PrCache::new)
}

/// The cached PR lookup of a task's worktree, never running `gh`.
pub(crate) fn pr_peek(task: &Task) -> Option<Value> {
    let wt = task.worktree_path.as_deref()?;
    pr_cache()
        .peek(Path::new(wt))
        .and_then(|l| serde_json::to_value(l).ok())
}

/// `task.pr {task, refresh?}`: PR status through `gh` (cached 60 s; only when `gh` is
/// installed and authenticated; never prompts).
pub(crate) async fn task_pr(server: &Arc<Server>, p: &Value) -> R {
    let t = req(p, "task")?;
    let task = owned_task(server, t)?;
    let wt = PathBuf::from(
        task.worktree_path
            .clone()
            .ok_or_else(|| invalid("the task has no checkout"))?,
    );
    let refresh = p.get("refresh").and_then(Value::as_bool).unwrap_or(false);
    let lookup = tokio::task::spawn_blocking(move || pr_cache().get(&wt, refresh))
        .await
        .map_err(internal)?;
    Ok(json!({"task": task.id, "pr": lookup}))
}

/// Per-server memory of what the loop already announced, so a stable problem is reported once.
#[derive(Default)]
pub(crate) struct ReconcileState {
    /// (repo, orphan path)
    orphans: HashSet<(PathBuf, PathBuf)>,
    moved: HashSet<(String, Option<String>)>,
}

/// Compare every owned worktree task with the worktrees on disk, repo by repo. Marks tasks
/// `missing` (and back to `active` when the checkout reappears), and emits events for missing
/// checkouts, moved branches and orphans. Returns the per-repo reports.
pub(crate) fn reconcile_once(
    server: &Arc<Server>,
    state: &mut ReconcileState,
    only_repo: Option<&Path>,
) -> Vec<Value> {
    let cfg = load_tasks_cfg();
    let root = vk_tasks::WorktreeRoot::parse(&cfg.root).unwrap_or(vk_tasks::WorktreeRoot::Sibling);
    let tasks: Vec<Task> = server.with_core(|c| {
        c.model
            .tasks
            .iter()
            .filter(|t| {
                t.ownership == TaskOwnership::Owned
                    && t.checkout.as_deref().is_none_or(|k| k == "worktree")
                    && t.worktree_path.is_some()
                    && !matches!(t.status.as_str(), "archived" | "removed" | "removing")
            })
            .cloned()
            .collect()
    });
    let mut repos: Vec<PathBuf> = tasks.iter().map(|t| PathBuf::from(&t.repo_root)).collect();
    if let Some(r) = only_repo {
        repos.push(r.to_path_buf());
    }
    repos.sort();
    repos.dedup();
    let mut reports = Vec::new();
    for repo in repos {
        if only_repo.is_some_and(|r| r != repo) {
            continue;
        }
        let mine: Vec<&Task> = tasks
            .iter()
            .filter(|t| Path::new(&t.repo_root) == repo)
            .collect();
        let tracked: Vec<vk_tasks::TrackedCheckout> = mine
            .iter()
            .map(|t| vk_tasks::TrackedCheckout {
                task_id: t.id.clone(),
                path: PathBuf::from(t.worktree_path.clone().unwrap_or_default()),
                branch: t.branch.clone(),
            })
            .collect();
        let Ok(rep) = vk_tasks::reconcile(&repo, &root, &tracked) else {
            continue;
        };
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        let mut changed = false;
        for m in &rep.missing {
            if let Some(mut t) = c.task(&m.task_id).cloned()
                && t.status != "missing"
            {
                t.status = "missing".into();
                tx.event(
                    "task.missing",
                    json!({"task": t.id}),
                    json!({"path": m.path, "reason": m.reason, "hint": "recreate it from its branch, or forget the task"}),
                );
                tx.task(t);
                changed = true;
            }
        }
        for t in &mine {
            let still_missing = rep.missing.iter().any(|m| m.task_id == t.id);
            if !still_missing
                && let Some(mut cur) = c.task(&t.id).cloned()
                && cur.status == "missing"
            {
                cur.status = "active".into();
                tx.event(
                    "task.recovered",
                    json!({"task": cur.id}),
                    json!({"path": cur.worktree_path}),
                );
                tx.task(cur);
                changed = true;
            }
        }
        state.moved.retain(|(t, a)| {
            !mine.iter().any(|m| &m.id == t)
                || rep
                    .branch_moved
                    .iter()
                    .any(|b| &b.task_id == t && &b.actual == a)
        });
        for b in &rep.branch_moved {
            if !state.moved.insert((b.task_id.clone(), b.actual.clone())) {
                continue;
            }
            changed = true;
            tx.event(
                "task.branch_changed",
                json!({"task": b.task_id}),
                json!({"path": b.path, "expected": b.expected, "actual": b.actual}),
            );
        }
        for o in &rep.orphans {
            if state.orphans.insert((repo.clone(), o.path.clone())) {
                changed = true;
                tx.event(
                    "worktree.orphan_found",
                    json!({"repo": repo}),
                    json!({"path": o.path, "kind": o.kind, "branch": o.branch,
                           "hint": "adopt it with `vibeke task adopt --path`, or remove it yourself; Vibeke never deletes it"}),
                );
            }
        }
        state
            .orphans
            .retain(|(r, p)| r != &repo || rep.orphans.iter().any(|o| &o.path == p));
        if changed {
            let _ = server.commit(&mut c, tx);
        }
        drop(c);
        reports.push(json!({"repo": repo, "missing": rep.missing, "branch_moved": rep.branch_moved, "orphans": rep.orphans}));
    }
    reports
}

/// `task.reconcile {repo?}`: run the reconcile now and return the reports.
pub(crate) async fn task_reconcile(server: &Arc<Server>, p: &Value) -> R {
    let repo = s(p, "repo").map(|r| {
        vk_tasks::repo_root(Path::new(r))
            .map(|i| i.root)
            .unwrap_or_else(|| PathBuf::from(r))
    });
    let srv = server.clone();
    let reports = tokio::task::spawn_blocking(move || {
        reconcile_once(&srv, &mut ReconcileState::default(), repo.as_deref())
    })
    .await
    .map_err(internal)?;
    Ok(json!({"reports": reports}))
}

/// Reconcile on start and every 60 s while there are tasks (05 §4).
pub fn start(server: &Arc<Server>) {
    let srv = server.clone();
    tokio::spawn(async move {
        let mut state = ReconcileState::default();
        loop {
            let has_tasks = srv.with_core(|c| {
                c.model
                    .tasks
                    .iter()
                    .any(|t| t.ownership == TaskOwnership::Owned)
            });
            if has_tasks {
                let s2 = srv.clone();
                state = tokio::task::spawn_blocking(move || {
                    reconcile_once(&s2, &mut state, None);
                    state
                })
                .await
                .unwrap_or_default();
            }
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    });
}

/// One materialized file for the response and the `task.files_materialized` event: its
/// path, what happened and (for copies) a hash. Never contents.
pub(crate) fn file_json(r: &vk_tasks::CopyResult) -> Value {
    use vk_tasks::CopyOutcome as O;
    let (outcome, method, error) = match &r.outcome {
        O::Copied => ("copied", None, None),
        O::Linked => ("linked", None, None),
        O::Cloned(m) => ("cloned", serde_json::to_value(m).ok(), None),
        O::MissingSource => ("missing", None, None),
        O::DestExists => ("exists", None, None),
        O::Rejected(e) => ("rejected", None, Some(e.clone())),
        O::Failed(e) => ("failed", None, Some(e.clone())),
    };
    json!({"path": r.rel, "outcome": outcome, "method": method, "hash": r.hash, "error": error})
}

/// State of the machine-wide port pool for `vibeke doctor` (05 §6): exhaustion, overlap with the
/// OS ephemeral range, leases outside the pool. Reads the lease table; never changes it.
pub fn port_health() -> Result<vk_tasks::PoolHealth, String> {
    let cfg = load_tasks_cfg();
    let pool = vk_tasks::PortPool {
        start: cfg.port_pool.start,
        end: cfg.port_pool.end,
        block: cfg.port_block,
    };
    vk_tasks::PortLeases::new(crate::paths::state_root(), pool)
        .health()
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease(start: u16) -> vk_tasks::Lease {
        vk_tasks::Lease {
            start,
            end: start + 9,
            task_id: "t1".into(),
            session: "s".into(),
            owner_pid: None,
            created_at: 0,
        }
    }

    fn repo_with(task_toml: &str) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".vibeke")).unwrap();
        std::fs::write(d.path().join(".vibeke/task.toml"), task_toml).unwrap();
        d
    }

    fn plan_for(
        dir: &Path,
        branch: &str,
        cfg: &vk_config::Tasks,
        trusted: bool,
    ) -> (SetupPlan, vk_tasks::Lease) {
        let resolved = resolve(dir, dir, cfg);
        let l = lease(20000);
        let vars = vars_for("t1", "fix", Some(branch), Some(&l), dir, dir);
        let plan = plan_setup(&resolved, None, &vars, dir, None, "", trusted);
        (plan, l)
    }

    fn user_override(cfg: &mut vk_config::Tasks, dir: &Path, v: Value) {
        cfg.repos.insert(
            dir.to_string_lossy().into_owned(),
            serde_json::from_value(v).unwrap(),
        );
    }

    /// Finding: an untrusted `[ports] env` naming `ZDOTDIR` pointed the pane's zsh at a repo
    /// directory (`20000/.zshenv`), running repo code at shell startup before trust.
    #[test]
    fn untrusted_ports_env_cannot_hijack_shell_startup() {
        let repo = repo_with(
            "[ports]\nenv = {ZDOTDIR = 0, BASH_ENV = 1, ENV = 1, API_PORT = 2, PATH = 3}\n",
        );
        let dir = repo.path();
        let sentinel = dir.join("SENTINEL");
        let touch = format!("touch '{}'\n", sentinel.display());
        std::fs::create_dir_all(dir.join("20000")).unwrap();
        std::fs::write(dir.join("20000/.zshenv"), &touch).unwrap();
        std::fs::write(dir.join("20001"), &touch).unwrap();
        for trusted in [false, true] {
            let (plan, _) = plan_for(dir, "main", &vk_config::Tasks::default(), trusted);
            let names: Vec<&str> = plan.env.iter().map(|(k, _)| k.as_str()).collect();
            assert!(names.contains(&"API_PORT"), "{names:?}");
            for bad in ["ZDOTDIR", "BASH_ENV", "ENV", "PATH"] {
                assert!(!names.contains(&bad), "trusted={trusted}: {names:?}");
            }
            // A shell started with exactly this env never runs the repo file.
            for sh in ["/bin/zsh", "/bin/bash", "/bin/sh"] {
                if !Path::new(sh).exists() {
                    continue;
                }
                let _ = std::process::Command::new(sh)
                    .args(["-c", "true"])
                    .current_dir(dir)
                    .env_remove("ZDOTDIR")
                    .env_remove("BASH_ENV")
                    .env_remove("ENV")
                    .envs(plan.env.iter().cloned())
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
                assert!(!sentinel.exists(), "{sh} ran repo startup code");
            }
        }
        // The probe is real: exporting the old mapping does run the file.
        if Path::new("/bin/zsh").exists() {
            let _ = std::process::Command::new("/bin/zsh")
                .args(["-c", "true"])
                .current_dir(dir)
                .env("ZDOTDIR", "20000")
                .status();
            assert!(sentinel.exists(), "probe should detect ZDOTDIR startup");
        }
        // A name the user declared in their own override passes; a denied one never does.
        let mut cfg = vk_config::Tasks::default();
        user_override(
            &mut cfg,
            dir,
            json!({"ports": {"env": {"DEV_SERVER": 3, "ZDOTDIR": 0}}}),
        );
        let (plan, _) = plan_for(dir, "main", &cfg, false);
        let names: Vec<&str> = plan.env.iter().map(|(k, _)| k.as_str()).collect();
        assert!(names.contains(&"DEV_SERVER"), "{names:?}");
        assert!(!names.contains(&"ZDOTDIR"), "{names:?}");
    }

    /// Finding: `echo "{branch}"` with a branch like `feature/$(…)` executed the substitution.
    /// Setup commands now reference `$VIBEKE_BRANCH`; run them through the real host runner.
    #[test]
    fn setup_commands_print_hostile_branch_names_literally() {
        let branch = "feature/$(printf${IFS}P1_INJECTED)'$(touch pwned)'`touch pwned2`";
        // `[env]` cannot redefine a template variable.
        let repo = repo_with("[env]\nVIBEKE_BRANCH = \"$(touch pwned3)\"\n");
        let dir = repo.path();
        let mut cfg = vk_config::Tasks::default();
        user_override(
            &mut cfg,
            dir,
            json!({"setup": {"run": [
                "printf '%s\\n' \"{branch}\" > out-double",
                "printf '%s\\n' '{branch}' > out-single",
                "printf '%s\\n' {branch} > out-bare",
            ]}}),
        );
        let (plan, l) = plan_for(dir, branch, &cfg, true);
        assert!(!plan.needs_trust);
        assert!(
            plan.commands
                .iter()
                .all(|c| !c.command.contains("P1_INJECTED"))
        );
        let opts = setup_options(&plan, "t1", dir, Some(l), dir.join("setup.log"));
        let out = vk_tasks::run_setup(&opts, &vk_tasks::CancelToken::default()).unwrap();
        assert!(
            matches!(out.status, vk_tasks::SetupStatus::Succeeded),
            "{:?}: {}",
            out.status,
            std::fs::read_to_string(dir.join("setup.log")).unwrap_or_default()
        );
        for f in ["out-double", "out-single", "out-bare"] {
            assert_eq!(
                std::fs::read_to_string(dir.join(f)).unwrap(),
                format!("{branch}\n"),
                "{f}"
            );
        }
        for f in ["pwned", "pwned2", "pwned3"] {
            assert!(!dir.join(f).exists(), "{f}");
        }
    }
}
