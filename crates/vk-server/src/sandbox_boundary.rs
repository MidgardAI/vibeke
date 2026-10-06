//! Boundary actions (13 §6, §8, §10, §12): what a contained run may not do itself is done by
//! the host after the user agrees. A box has no git push credentials and no way to write the
//! host outside its checkout, so:
//!
//! - `sandbox.request {kind: push|copy_out, remote?, path?}` (through the broker, or for a pane
//!   from a user client) opens an approval Interaction on the requesting pane. On "allow" the
//!   host pushes the task branch (after pulling a container clone) with hooks off, or copies
//!   one regular file from the checkout/clone into `<state>/outbox/<box>/`. The result is a
//!   `sandbox.boundary_action {kind, interaction, outcome}` event (`applied`, `failed`,
//!   `denied`, `expired`), following the delivery states of 04 §7.3.
//! - `sandbox.push {task, remote?}` and `sandbox.copy_out {task, path}` are the same actions
//!   started by the user (no Interaction).
//!
//! The pushed ref is always the task's own branch (`refs/heads/<b>:refs/heads/<b>`); a request
//! cannot name another ref or a refspec.

use super::*;

const BOUNDARY_TTL: Duration = Duration::from_secs(10 * 60);
const COPY_MAX: u64 = 100 * 1024 * 1024;

/// The context that contains `pane` (task box, or the pane's run-scoped box).
fn box_of_pane(server: &Server, pane: &str) -> Option<Arc<TaskBox>> {
    let task = server.with_core(|c| {
        c.pane(pane)
            .and_then(|p| c.ws(&p.workspace))
            .and_then(|w| w.task.clone())
    });
    task.and_then(|t| server.sandbox.get(&t))
        .or_else(|| server.sandbox.get(&format!("pane:{pane}")))
}

fn valid_ref_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 200
        && !s.starts_with('-')
        && !s.contains("..")
        && !s.ends_with(".lock")
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
}

/// Push the context's task branch to `remote` from the host (blocking).
pub fn do_push(server: &Server, tb: &TaskBox, remote: &str) -> Result<Value, String> {
    if !valid_ref_name(remote) {
        return Err(format!("invalid remote name {remote:?}"));
    }
    let mut synced = Value::Null;
    let (repo, branch) = match &tb.runner {
        BoxRunner::Container(c) if c.clone.is_some() => {
            let cl = c.clone.as_ref().unwrap();
            // The commits live in the box's clone: bring them to the host branch first.
            let o = container::sync_task(c, "pull", false).map_err(|e| e.message)?;
            container::record_sync(server, tb, &o);
            synced = serde_json::to_value(&o).unwrap_or_default();
            (cl.repo.clone(), cl.branch.clone())
        }
        _ => {
            let repo = vk_tasks::repo_root(&tb.checkout)
                .map(|r| r.root)
                .ok_or("the checkout is not a git repository")?;
            let branch = vk_sandbox::GitLayout::detect(&tb.checkout)
                .and_then(|l| l.branch)
                .filter(|b| b != "HEAD")
                .ok_or("the checkout has no branch (detached HEAD)")?;
            (repo, branch)
        }
    };
    if !valid_ref_name(&branch) {
        return Err(format!("refusing to push branch {branch:?}"));
    }
    let safety = vk_tasks::safety_args(&repo).map_err(|e| e.to_string())?;
    let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(&safety)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "push",
            "--",
            remote,
            &refspec,
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if !out.status.success() {
        return Err(format!("git push failed: {stderr}"));
    }
    Ok(json!({"remote": remote, "branch": branch, "synced": synced, "output": stderr}))
}

/// Copy one regular file (relative to the checkout or the box's clone) into the host outbox
/// (blocking). Symlinks anywhere on the path, `..` and absolute paths are refused.
pub fn do_copy_out(tb: &TaskBox, rel: &str) -> Result<PathBuf, String> {
    let relp = Path::new(rel.trim_start_matches("./"));
    let ok = !rel.is_empty()
        && relp
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)));
    if !ok {
        return Err(format!(
            "{rel}: give a path relative to the checkout (no .., not absolute)"
        ));
    }
    let root = match &tb.runner {
        BoxRunner::Container(c) => c
            .clone
            .as_ref()
            .map(|cl| cl.dir.clone())
            .unwrap_or_else(|| tb.checkout.clone()),
        _ => tb.checkout.clone(),
    };
    let src = root.join(relp);
    if !vk_sandbox::fsafe::contained_no_symlink(&root, &src) {
        return Err(format!(
            "{rel}: not inside the checkout, or a symlink is on the way"
        ));
    }
    let md = std::fs::symlink_metadata(&src).map_err(|e| format!("{rel}: {e}"))?;
    if !md.is_file() {
        return Err(format!("{rel}: not a regular file"));
    }
    if md.len() > COPY_MAX {
        return Err(format!(
            "{rel}: larger than {} MB",
            COPY_MAX / (1024 * 1024)
        ));
    }
    let bytes = std::fs::read(&src).map_err(|e| e.to_string())?;
    let outbox = paths::state_root().join("outbox");
    std::fs::create_dir_all(&outbox).map_err(|e| e.to_string())?;
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&outbox, std::fs::Permissions::from_mode(0o700));
    }
    let dir = vk_sandbox::fsafe::ensure_dir_under(
        &outbox,
        Path::new(&vk_sandbox::runner::short_id(&tb.key)),
    )
    .map_err(|e| e.to_string())?;
    let name = relp
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let mut dst = dir.join(&name);
    let mut n = 1;
    while std::fs::symlink_metadata(&dst).is_ok() {
        dst = dir.join(format!("{n}-{name}"));
        n += 1;
    }
    vk_sandbox::fsafe::write_nofollow(&dst, &bytes, 0o600).map_err(|e| e.to_string())?;
    Ok(dst)
}

fn record(
    server: &Server,
    tb: &TaskBox,
    kind: &str,
    interaction: Option<&str>,
    outcome: &str,
    detail: Value,
) {
    emit(
        server,
        "sandbox.boundary_action",
        json!({"task": tb.task, "sandbox": tb.key}),
        json!({"kind": kind, "interaction": interaction, "outcome": outcome, "detail": detail}),
    );
}

async fn perform(
    server: &Arc<Server>,
    tb: Arc<TaskBox>,
    kind: String,
    arg: String,
) -> (String, Value) {
    let srv = server.clone();
    let r = tokio::task::spawn_blocking(move || match kind.as_str() {
        "push" => do_push(&srv, &tb, &arg),
        _ => do_copy_out(&tb, &arg).map(|p| json!({"path": p})),
    })
    .await
    .unwrap_or_else(|e| Err(e.to_string()));
    match r {
        Ok(v) => ("applied".into(), v),
        Err(e) => ("failed".into(), json!({"error": e})),
    }
}

/// `sandbox.request`: a contained run asks for a boundary action.
async fn request(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let pane = match (&ctx.pane_scope, s(p, "pane")) {
        (Some(own), _) => own.clone(),
        (None, Some(t)) => crate::api::resolve_pane(server, ctx, Some(t))?.id,
        (None, None) => return Err(invalid("pane required")),
    };
    let kind = crate::api::req(p, "kind")?.to_string();
    let tb = box_of_pane(server, &pane)
        .ok_or_else(|| invalid("boundary actions are for contained panes (13 §8)"))?;
    let (title, summary, arg, risk) = match kind.as_str() {
        "push" => {
            let remote = s(p, "remote").unwrap_or("origin").to_string();
            if !valid_ref_name(&remote) {
                return Err(invalid(format!("invalid remote name {remote:?}")));
            }
            let branch = match &tb.runner {
                BoxRunner::Container(c) => c.clone.as_ref().map(|cl| cl.branch.clone()),
                _ => None,
            }
            .or_else(|| vk_sandbox::GitLayout::detect(&tb.checkout).and_then(|l| l.branch))
            .unwrap_or_else(|| "?".into());
            (
                format!("Push {branch} to {remote}?"),
                format!("git push {remote} {branch} (from the host, hooks off)"),
                remote,
                Risk::High,
            )
        }
        "copy_out" => {
            let path = crate::api::req(p, "path")?.to_string();
            (
                format!("Copy {path} out of the box?"),
                format!("copy {path} to the host outbox"),
                path,
                Risk::Medium,
            )
        }
        other => {
            return Err(invalid(format!(
                "unknown boundary action {other} (push|copy_out)"
            )));
        }
    };
    let run = server
        .with_core(|c| c.run_for_pane(&pane).map(|r| r.id.clone()))
        .unwrap_or_default();
    let id = ulid();
    let it = {
        let mut c = server.core.lock().unwrap();
        let it = Interaction {
            id: id.clone(),
            handle: c.next_interaction_handle(),
            run: run.clone(),
            pane: pane.clone(),
            kind: InteractionKind::Approval,
            status: InteractionStatus::Open,
            title: title.clone(),
            body_md: Some(format!(
                "A contained run asks the host to act across the box boundary (13 §8): {summary}.\n\n**allow** = do it now · **deny**"
            )),
            action: Some(ActionInfo {
                tool: "boundary".into(),
                summary: summary.clone(),
                command: None,
                paths: if kind == "copy_out" {
                    vec![arg.clone()]
                } else {
                    vec![]
                },
                diff: None,
                risk,
                risk_reasons: vec![format!("boundary action: {kind}")],
            }),
            questions: vec![],
            plan_md: None,
            answer_channel: AnswerChannel::Native,
            native_ref: Some(format!("boundary:{kind}:{}", tb.key)),
            source: StateSource::Structured,
            confidence: 1.0,
            answerable: true,
            gate: true,
            decision_rev: 0,
            delivery: DeliveryState::None,
            delivery_error: None,
            answer: None,
            answered_by: None,
            answer_key: None,
            opened_at_ms: vk_store::now_ms(),
            answered_at_ms: None,
        };
        let mut t = Tx::new();
        t.counters = true;
        t.event(
            "interaction.opened",
            json!({"interaction": id, "pane": pane, "run": run}),
            json!({"kind": "approval", "source": "sandbox", "boundary": {"kind": kind}}),
        );
        t.interaction(it.clone());
        server.commit(&mut c, t).map_err(internal)?;
        it
    };
    server.notify(
        "interaction",
        Some(&pane),
        "boundary action requested",
        &it.title,
        "normal",
    );
    let gate = crate::agents::hold_external_gate(server, &id, &pane);
    let srv = server.clone();
    let (id2, kind2) = (id.clone(), kind.clone());
    tokio::spawn(async move {
        let answered = tokio::time::timeout(BOUNDARY_TTL, gate).await;
        let allowed = matches!(answered, Ok(Ok(true)))
            && srv
                .with_core(|c| {
                    c.interaction(&id2)
                        .and_then(|i| i.answer.as_ref()?.decision)
                })
                .is_some_and(|d| matches!(d, Decision::Allow | Decision::AllowAlways));
        let (outcome, detail) = match answered {
            Ok(Ok(true)) if allowed => perform(&srv, tb.clone(), kind2.clone(), arg).await,
            Ok(Ok(true)) => ("denied".into(), Value::Null),
            Ok(_) => ("cancelled".into(), Value::Null),
            Err(_) => ("expired".into(), Value::Null),
        };
        crate::agents::close_interaction(
            &srv,
            &id2,
            if outcome == "expired" {
                InteractionStatus::Expired
            } else {
                InteractionStatus::Answered
            },
            &format!("boundary action {outcome}"),
        );
        record(&srv, &tb, &kind2, Some(&id2), &outcome, detail);
    });
    Ok(json!({"interaction": id, "kind": kind, "status": "pending"}))
}

fn task_box(server: &Server, p: &Value) -> Result<Arc<TaskBox>, vk_proto::rpc::RpcError> {
    let t = crate::api::req(p, "task")?;
    let key = server
        .with_core(|c| c.task(t).map(|x| x.id.clone()))
        .unwrap_or_else(|| t.to_string());
    server
        .sandbox
        .get(&key)
        .ok_or_else(|| crate::api::not_found("sandbox", t))
}

/// Methods served here (dispatched from `sandbox::api`).
pub async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    Some(match method {
        "sandbox.request" => request(server, ctx, p).await,
        "sandbox.push" | "sandbox.copy_out" => {
            if ctx.pane_scope.is_some() {
                return Some(Err(err(
                    ErrorKind::PermissionDenied,
                    format!("{method} needs a user client (a contained run uses sandbox.request)"),
                )));
            }
            let tb = match task_box(server, p) {
                Ok(b) => b,
                Err(e) => return Some(Err(e)),
            };
            let (kind, arg) = if method == "sandbox.push" {
                ("push", s(p, "remote").unwrap_or("origin").to_string())
            } else {
                match crate::api::req(p, "path") {
                    Ok(x) => ("copy_out", x.to_string()),
                    Err(e) => return Some(Err(e)),
                }
            };
            let (outcome, detail) = perform(server, tb.clone(), kind.into(), arg).await;
            record(server, &tb, kind, None, &outcome, detail.clone());
            if outcome == "applied" {
                Ok(json!({"kind": kind, "outcome": outcome, "detail": detail}))
            } else {
                Err(err(
                    ErrorKind::Conflict,
                    detail["error"].as_str().unwrap_or("failed").to_string(),
                )
                .details(json!({"kind": kind, "outcome": outcome})))
            }
        }
        _ => return None,
    })
}
