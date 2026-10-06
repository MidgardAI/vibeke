//! Spec 15 T4, server side: validated dirty snapshots (§5), reviewer runs and their findings
//! as attributed notes (§6.1, §7), confirmed dependency links (§8.1, §10.2) and the effort
//! heuristic (§8.2). The model-backed effort estimate is the `effort_estimate` operation of
//! `crate::assist` (preview + confirm; its result is applied only by an explicit `task.set`).
//!
//! Authority (15 §11): snapshots, reviewer runs, note classification and dependency edges are
//! human-client mutations (forbidden for pane scope in `api::authorize`); reads are authorized
//! per task before anything is retrieved.

use super::*;
use vk_review::Actor;
use vk_review::binding::{self, BindRequest, IdentityEvidence};
use vk_review::dependency::{self as dep, DependencyEdge, DependencyKind};
use vk_review::effort::{self, EffortInputs};
use vk_review::reviewer::{self, Classification, PromptInput, ReviewNote};
use vk_review::snapshot::{self, SnapshotOptions};

pub(super) const K_SNAP: &str = "review_snapshot";
pub(super) const K_DEP: &str = "task_dependency";
pub(super) const K_NOTE: &str = "review_note";
pub(super) const K_REVREQ: &str = "reviewer_request";

pub const METHODS: &[(&str, bool)] = &[
    ("task.review.snapshot", true),
    ("task.review.request_reviewer", true),
    ("task.review.start_reviewer", true),
    ("task.review.notes", false),
    ("task.review.note.classify", true),
    ("task.dependency.add", true),
    ("task.dependency.remove", true),
    ("task.dependency.list", false),
    ("task.effort.estimate", false),
];

// ---- records ------------------------------------------------------------------------------------

/// A validated dirty snapshot taken for a task (keyed `task:subject`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapRec {
    pub task: String,
    pub subject_id: String,
    pub head_sha: String,
    pub dirty_digest: String,
    pub content_sha: String,
    pub attempts: u32,
    pub created_by: Actor,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewerState {
    /// Prompt shown; nothing launched.
    Prepared,
    Started,
    Failed,
}

/// An explicit reviewer-run request: the exact prompt the user confirms, then the run and its
/// `review` binding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewerRequest {
    pub id: String,
    pub task: String,
    pub subject_id: String,
    pub harness: String,
    pub prompt: String,
    pub prompt_digest: String,
    /// `generated` (deterministic from the package) or `user_edited`.
    pub prompt_source: String,
    pub state: ReviewerState,
    pub created_by: Actor,
    pub created_at_ms: i64,
    #[serde(default)]
    pub run: Option<String>,
    #[serde(default)]
    pub pane: Option<String>,
    #[serde(default)]
    pub binding: Option<String>,
    #[serde(default)]
    pub started_at_ms: Option<i64>,
    #[serde(default)]
    pub error: Option<String>,
}

// ---- helpers used by review.rs ------------------------------------------------------------------

fn snaps_of(c: &Core, task: &str) -> Vec<SnapRec> {
    let mut v: Vec<SnapRec> = by_task(c, K_SNAP, task);
    v.sort_by_key(|s| s.created_at_ms);
    v
}

/// The task's latest snapshot when it still describes the live checkout exactly (same HEAD,
/// same complete change digest): it is then the current, accept-capable candidate.
pub(super) fn current_snapshot(
    server: &Server,
    task: &str,
    head: Option<&str>,
    digest: Option<&str>,
) -> Option<ChangeSubject> {
    let (head, digest) = (head?, digest?);
    server.with_core(|c| {
        let last = snaps_of(c, task).pop()?;
        (last.head_sha == head && last.dirty_digest == digest)
            .then(|| {
                c.store
                    .get::<ChangeSubject>(K_SUBJECT, &last.subject_id)
                    .ok()
                    .flatten()
            })
            .flatten()
            .filter(|s| s.is_immutable() && s.verify_id())
    })
}

/// Every snapshot subject recorded for the task (oldest first).
pub(super) fn snapshot_subjects(server: &Server, task: &str) -> Vec<ChangeSubject> {
    server.with_core(|c| {
        snaps_of(c, task)
            .into_iter()
            .filter_map(|r| {
                c.store
                    .get::<ChangeSubject>(K_SUBJECT, &r.subject_id)
                    .ok()
                    .flatten()
            })
            .collect()
    })
}

pub(super) fn notes_of_c(c: &Core, task: &str) -> Vec<ReviewNote> {
    let mut v: Vec<ReviewNote> = by_task(c, K_NOTE, task);
    v.sort_by(|a, b| (a.created_at_ms, &a.id).cmp(&(b.created_at_ms, &b.id)));
    v.dedup_by(|a, b| a.id == b.id);
    v
}

pub(super) fn notes_of(server: &Server, task: &str) -> Vec<ReviewNote> {
    server.with_core(|c| notes_of_c(c, task))
}

/// Unassessed potentially blocking reviewer findings and user-marked blockers on `subject`
/// (15 §7): they keep the stronger readiness label away.
pub(super) fn open_concerns(notes: &[ReviewNote], subject: Option<&str>) -> Vec<String> {
    notes
        .iter()
        .filter(|n| n.is_open_concern())
        .filter(|n| n.subject_id.is_none() || n.subject_id.as_deref() == subject)
        .map(|n| n.id.clone())
        .collect()
}

/// State-token contribution (15 §7 known competing update): snapshots taken and note
/// classifications.
pub(super) fn token_part(c: &Core, task: &str) -> String {
    let snaps: Vec<String> = snaps_of(c, task)
        .into_iter()
        .map(|s| s.subject_id)
        .collect();
    let notes: Vec<String> = notes_of_c(c, task)
        .into_iter()
        .map(|n| format!("{}:{:?}", n.id, n.classification))
        .collect();
    format!("snapshots {snaps:?}\nnotes {notes:?}\n")
}

/// Reviewer (role `review`) bindings never contribute observed commands or claims to the
/// implementation's evidence; their findings are notes.
pub(super) fn evidence_bindings(bs: &[TaskRunBinding]) -> Vec<TaskRunBinding> {
    bs.iter()
        .filter(|b| b.role != BindingRole::Review)
        .cloned()
        .collect()
}

fn dep_edges(c: &Core) -> Vec<DependencyEdge> {
    c.store.load::<DependencyEdge>(K_DEP).unwrap_or_default()
}

/// `task id → open tasks it blocks` through confirmed edges (attention ranking input).
pub(super) fn blocked_counts(c: &Core) -> HashMap<String, usize> {
    let edges = dep_edges(c);
    if edges.is_empty() {
        return HashMap::new();
    }
    let open: HashSet<String> = c
        .model
        .tasks
        .iter()
        .filter(|t| matches!(t.status.as_str(), "active" | "parked"))
        .map(|t| t.id.clone())
        .collect();
    dep::blocked_dependents(&edges, |t| open.contains(t))
        .into_iter()
        .map(|(k, v)| (k, v.len()))
        .collect()
}

/// Deterministic effort heuristic for a package (labelled `heuristic`, never applied).
pub(super) fn effort_heuristic(
    diff: Option<&subject::DiffStat>,
    runs: &[CheckRunRec],
    selected: Option<&ChangeSubject>,
    intent: Option<&TaskIntent>,
) -> effort::EffortEstimate {
    let mut latest: BTreeMap<&str, &CheckRun> = BTreeMap::new();
    if let Some(s) = selected {
        for r in runs
            .iter()
            .filter(|r| r.run.subject_id == s.id && r.run.state.is_terminal())
        {
            latest.insert(r.run.check_id.as_str(), &r.run);
        }
    }
    let inputs = EffortInputs {
        files: diff.map_or(0, |d| d.files.len()),
        lines: diff.map_or(0, |d| d.added + d.removed),
        binary_files: diff.map_or(0, |d| d.files.iter().filter(|f| f.added.is_none()).count()),
        failing_checks: latest
            .values()
            .filter(|r| r.state != CheckState::Passed)
            .count(),
        human_criteria: intent.map_or(0, |i| {
            i.criteria
                .iter()
                .filter(|c| c.evaluation == Evaluation::Human)
                .count()
        }),
    };
    effort::heuristic(&inputs)
}

pub(super) fn dependencies_json(server: &Server, task: &str) -> Value {
    server.with_core(|c| {
        let edges = dep_edges(c);
        let counts = blocked_counts(c);
        let title = |id: &str| c.task(id).map(|t| t.title.clone());
        json!({
            "depends_on": edges.iter().filter(|e| e.task == task).map(|e| json!({"edge": e, "title": title(&e.depends_on)})).collect::<Vec<_>>(),
            "dependents": edges.iter().filter(|e| e.depends_on == task).map(|e| json!({"edge": e, "title": title(&e.task)})).collect::<Vec<_>>(),
            "blocks_open_tasks": counts.get(task).copied().unwrap_or(0),
            "note": "Confirmed links only; inferred relationships are never ranked.",
        })
    })
}

pub(super) fn note_json(n: &ReviewNote) -> Value {
    let mut v = serde_json::to_value(n).unwrap_or(Value::Null);
    v["label"] = json!(n.label());
    v["open_concern"] = json!(n.is_open_concern());
    v
}

pub(super) fn reviewer_requests(server: &Server, task: &str) -> Vec<ReviewerRequest> {
    server.with_core(|c| {
        let mut v: Vec<ReviewerRequest> = by_task(c, K_REVREQ, task);
        v.sort_by_key(|r| r.created_at_ms);
        v.dedup_by(|a, b| a.id == b.id);
        v
    })
}

pub(super) fn reviewer_json(r: &ReviewerRequest) -> Value {
    json!({
        "id": r.id, "subject": r.subject_id, "harness": r.harness, "state": r.state,
        "prompt_digest": r.prompt_digest, "prompt_source": r.prompt_source,
        "run": r.run, "pane": r.pane, "binding": r.binding, "created_at_ms": r.created_at_ms,
        "started_at_ms": r.started_at_ms, "error": r.error,
    })
}

// ---- API ----------------------------------------------------------------------------------------

pub(super) async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    Some(match method {
        "task.review.snapshot" => snapshot_api(server, ctx, p).await,
        "task.review.request_reviewer" => request_reviewer(server, ctx, p).await,
        "task.review.start_reviewer" => start_reviewer(server, ctx, p).await,
        "task.review.notes" => notes_api(server, ctx, p),
        "task.review.note.classify" => classify_api(server, ctx, p),
        "task.dependency.add" => dependency_add(server, ctx, p),
        "task.dependency.remove" => dependency_remove(server, ctx, p),
        "task.dependency.list" => dependency_list(server, ctx, p),
        "task.effort.estimate" => effort_api(server, ctx, p).await,
        _ => return None,
    })
}

/// `task.review.snapshot {task, idempotency_key?}`: capture the checkout's uncommitted work as
/// a validated, immutable snapshot subject (15 §5). Refused with `workspace_changing` when no
/// consistent capture could be made; the user's index, files and branches are never touched.
async fn snapshot_api(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    const M: &str = "task.review.snapshot";
    if let Some(r) = receipts::replay(server, ctx, M, p) {
        return r;
    }
    let task_id = req(p, "task")?;
    authorize_task(server, ctx, task_id)?;
    let task = tracking::find_task(server, task_id)?;
    let path = checkout_of(&task).ok_or_else(|| {
        conflict(
            "checkout_unavailable",
            "the task's checkout is unavailable; nothing to snapshot",
        )
    })?;
    let srv = server.clone();
    let (t2, p2) = (task.clone(), path.clone());
    let captured = blocking(move || {
        let (base, _) = base_for(&srv, &t2, &p2);
        let base = base.ok_or_else(|| "no review base could be resolved".to_string())?;
        snapshot::capture_dirty_snapshot(&p2, &base, &SnapshotOptions::default())
            .map_err(|e| (e.reason(), e.to_string()))
            .map_err(|(r, m)| format!("{r}\u{0}{m}"))
    })
    .await?;
    let subj = match captured {
        Ok(s) => s,
        Err(e) => {
            let (reason, msg) = e.split_once('\u{0}').unwrap_or(("snapshot_failed", &e));
            let mut d = json!({"reason": reason});
            if reason == "workspace_changing" {
                d["label"] = json!(snapshot::WORKSPACE_CHANGING);
                d["offers"] = json!(["select a committed revision", "snapshot again"]);
            }
            return Err(err(ErrorKind::Conflict, msg).details(d));
        }
    };
    let sn = subj.snapshot.clone().expect("dirty snapshot has content");
    let rec = SnapRec {
        task: task.id.clone(),
        subject_id: subj.id.clone(),
        head_sha: subj.head_sha.clone(),
        dirty_digest: subj.dirty_digest.clone().unwrap_or_default(),
        content_sha: sn.commit.clone(),
        attempts: sn.attempts,
        created_by: tracking::user(ctx),
        created_at_ms: now(),
    };
    let result = json!({
        "subject": subj,
        "snapshot": sn,
        "label": "Snapshot of uncommitted work · accept-capable while the checkout still matches it",
        "note": format!("Stored as an immutable commit under {}; your index, files and branches are unchanged.", sn.ref_name),
    });
    {
        let mut c = server.core.lock().unwrap();
        if let Some(r) = receipts::replay_in(&c, ctx, M, p) {
            return r;
        }
        let mut tx = Tx::new();
        if c.store
            .get::<Value>(K_SUBJECT, &subj.id)
            .ok()
            .flatten()
            .is_none()
        {
            tx.m.close(K_SUBJECT, &subj.id, None, &subj);
        }
        let key = format!("{}:{}", task.id, subj.id);
        if c.store.get::<Value>(K_CAND, &key).ok().flatten().is_none() {
            let cr = CandidateRec {
                task: task.id.clone(),
                subject_id: subj.id.clone(),
                head_sha: subj.head_sha.clone(),
                base_sha: subj.base_sha.clone(),
                source: "dirty_snapshot".into(),
                binding: None,
                created_at_ms: now(),
            };
            tx.m.close(K_CAND, &key, None, &cr);
            tx.event(
                "review.candidate_created",
                json!({"task": task.id}),
                json!({"subject": subj.id, "head": subj.head_sha, "source": "dirty_snapshot"}),
            );
        }
        tx.m.put(K_SNAP, &key, None, &rec);
        tx.event_by(
            "review.snapshot_created",
            json!({"task": task.id, "subject": subj.id}),
            json!({"kind": "user", "id": rec.created_by.id}),
            json!({"head": subj.head_sha, "content": sn.commit, "ref": sn.ref_name, "attempts": sn.attempts}),
        );
        receipts::record(&mut tx, ctx, M, p, &result);
        server.commit(&mut c, tx).map_err(internal)?;
    }
    spawn_refresh(server, &task.id);
    Ok(result)
}

/// `task.review.request_reviewer {task, harness?, subject?, prompt?}`: build the reviewable
/// prompt from the package and record it. Launches nothing and sends nothing; the user starts
/// the run with `task.review.start_reviewer` naming this exact prompt's digest.
async fn request_reviewer(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    const M: &str = "task.review.request_reviewer";
    if let Some(r) = receipts::replay(server, ctx, M, p) {
        return r;
    }
    let task_id = req(p, "task")?;
    authorize_task(server, ctx, task_id)?;
    let harness = s(p, "harness").unwrap_or("claude").to_string();
    if crate::agents::harness::Harness::from_id(&harness).is_none() {
        return Err(invalid(format!("unknown harness {harness}")));
    }
    let pkg = package(server, task_id, s(p, "subject")).await?;
    let subj = pkg.selected.clone().ok_or_else(|| {
        conflict(
            "no_subject",
            "nothing to review yet: commit or snapshot the work first",
        )
    })?;
    if !subj.is_immutable() {
        return Err(conflict(
            "verification_unbound",
            "a reviewer needs an immutable subject: select a committed revision or take a snapshot",
        ));
    }
    let (prompt, source) = match s(p, "prompt").filter(|t| !t.trim().is_empty()) {
        Some(t) => {
            let (t, _) = vk_assist::clip(t, reviewer::MAX_PROMPT_BYTES);
            (t, "user_edited")
        }
        None => (
            reviewer::build_prompt(&prompt_input(&pkg, &subj)),
            "generated",
        ),
    };
    let rq = ReviewerRequest {
        id: format!("rv_{}", crate::core::ulid().to_lowercase()),
        task: pkg.task.id.clone(),
        subject_id: subj.id.clone(),
        harness: harness.clone(),
        prompt_digest: reviewer::prompt_digest(&prompt),
        prompt: prompt.clone(),
        prompt_source: source.into(),
        state: ReviewerState::Prepared,
        created_by: tracking::user(ctx),
        created_at_ms: now(),
        run: None,
        pane: None,
        binding: None,
        started_at_ms: None,
        error: None,
    };
    let result = json!({
        "request": reviewer_json(&rq),
        "prompt": prompt,
        "prompt_digest": rq.prompt_digest,
        "harness": harness,
        "subject": subj.id,
        "requires_confirmation": true,
        "label": "Nothing is launched or sent until you confirm this exact prompt. The reviewer's findings are recorded as its opinion, never as evidence.",
        "uses_provider": format!("Runs {harness} with your own account; it consumes that provider's usage."),
        "confirm_with": {"method": "task.review.start_reviewer", "params": {"request": rq.id, "prompt_digest": rq.prompt_digest}},
    });
    let mut c = server.core.lock().unwrap();
    if let Some(r) = receipts::replay_in(&c, ctx, M, p) {
        return r;
    }
    let mut tx = Tx::new();
    tx.m.put(K_REVREQ, &rq.id, None, &rq);
    tx.event_by(
        "review.reviewer_requested",
        json!({"task": rq.task, "request": rq.id}),
        json!({"kind": "user", "id": rq.created_by.id}),
        json!({"subject": rq.subject_id, "harness": rq.harness, "prompt_digest": rq.prompt_digest, "prompt_bytes": rq.prompt.len(), "prompt_source": rq.prompt_source}),
    );
    receipts::record(&mut tx, ctx, M, p, &result);
    server.commit(&mut c, tx).map_err(internal)?;
    Ok(result)
}

fn prompt_input(pkg: &Pkg, subj: &ChangeSubject) -> PromptInput {
    let intent = pkg.intent.as_ref();
    let status = |id: &str| {
        pkg.assessment
            .criteria
            .iter()
            .find(|a| a.criterion_id == id)
            .and_then(|a| serde_json::to_value(a.status).ok())
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_else(|| "unknown".into())
    };
    let files = pkg.json["diff_stat"]["files"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|f| {
                    (
                        f["path"].as_str().unwrap_or("").to_string(),
                        f["added"].as_u64(),
                        f["removed"].as_u64(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let checks = pkg.json["check_runs"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|r| r["subject_id"] == subj.id.as_str())
                .map(|r| {
                    format!(
                        "{}: {}",
                        r["check_id"].as_str().unwrap_or("?"),
                        r["state"].as_str().unwrap_or("?")
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    PromptInput {
        task_title: pkg.task.title.clone(),
        objective: intent.map(|i| i.objective.clone()).unwrap_or_default(),
        constraints: intent
            .map(|i| i.constraints.iter().map(|c| c.text.clone()).collect())
            .unwrap_or_default(),
        criteria: intent
            .map(|i| {
                i.criteria
                    .iter()
                    .map(|c| (c.id.clone(), c.text.clone(), c.required, status(&c.id)))
                    .collect()
            })
            .unwrap_or_default(),
        stop_at: intent
            .and_then(|i| serde_json::to_value(i.stop_at).ok())
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_else(|| "unspecified".into()),
        subject_id: subj.id.clone(),
        subject_kind: serde_json::to_value(subj.kind)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default(),
        base_sha: subj.base_sha.clone(),
        content_sha: subj.content_sha().to_string(),
        files,
        checks,
    }
}

/// Launches the reviewer harness and returns `(run id, pane id)`.
async fn launch(
    server: &Arc<Server>,
    ctx: &Ctx,
    rq: &ReviewerRequest,
    p: &Value,
) -> Result<(String, String), RpcError> {
    #[cfg(test)]
    if let Some(f) = test_launcher(&rq.task) {
        return f(server, rq);
    }
    let task = tracking::find_task(server, &rq.task)?;
    let pane = match s(p, "pane") {
        Some(pn) => crate::api::resolve_pane(server, ctx, Some(pn))?,
        None => {
            let base = match s(p, "split_of") {
                Some(b) => crate::api::resolve_pane(server, ctx, Some(b))?,
                None => {
                    let pane_id = tracking::task_bindings(server, &task.id)
                        .into_iter()
                        .rev()
                        .filter(|b| b.role == BindingRole::Implementation)
                        .find_map(|b| {
                            server.with_core(|c| c.run(&b.run_id).map(|r| r.pane.clone()))
                        })
                        .ok_or_else(|| {
                            invalid("no pane to split: pass --pane (an empty shell) or --split-of")
                        })?;
                    server
                        .with_core(|c| c.pane(&pane_id).cloned())
                        .ok_or_else(|| not_found("pane", &pane_id))?
                }
            };
            let dir = vk_proto::layout::Direction::parse(s(p, "direction").unwrap_or("right"))
                .unwrap_or(vk_proto::layout::Direction::Right);
            let cwd = checkout_of(&task).map(|p| p.to_string_lossy().into_owned());
            // Never moves anyone's focus (15 §1.1).
            let pane = server
                .split_pane(
                    &base.id,
                    dir,
                    0.5,
                    cwd.as_deref(),
                    None,
                    Some("reviewer".into()),
                    None,
                    "user",
                )
                .map_err(internal)?;
            tokio::time::sleep(Duration::from_millis(400)).await;
            pane
        }
    };
    let v = crate::agents::start_in_pane(
        server,
        &pane.id,
        &rq.harness,
        None,
        Some(&rq.prompt),
        &[],
        None,
    )
    .await?;
    let run = v["run"]["id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| internal("the reviewer run did not start"))?;
    Ok((run, pane.id))
}

/// `task.review.start_reviewer {request, prompt_digest, pane? | split_of?, direction?}`: the
/// user's confirmation of the exact prompt. Starts the harness through the ordinary launch path
/// (the confirmed prompt is its initial prompt, sent once) and binds the run to the task with
/// role `review`.
async fn start_reviewer(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let id = req(p, "request")?;
    let digest = req(p, "prompt_digest")?;
    let rq = server
        .with_core(|c| c.store.get::<ReviewerRequest>(K_REVREQ, id).ok().flatten())
        .ok_or_else(|| not_found("reviewer request", id))?;
    authorize_task(server, ctx, &rq.task)?;
    if rq.state == ReviewerState::Started {
        return Ok(json!({"request": reviewer_json(&rq), "replayed": true}));
    }
    if rq.state != ReviewerState::Prepared {
        return Err(conflict(
            "not_prepared",
            "this reviewer request is not awaiting confirmation; request a new one",
        ));
    }
    if digest != rq.prompt_digest {
        return Err(conflict(
            "prompt_mismatch",
            "the confirmed prompt differs from the prepared one; nothing was launched",
        )
        .details(json!({"reason": "prompt_mismatch", "expected": rq.prompt_digest})));
    }
    let mut rq = rq;
    let (run_id, pane) = match launch(server, ctx, &rq, p).await {
        Ok(x) => x,
        Err(e) => {
            rq.state = ReviewerState::Failed;
            rq.error = Some(e.message.clone());
            let mut c = server.core.lock().unwrap();
            let mut tx = Tx::new();
            tx.m.close(K_REVREQ, &rq.id, None, &rq);
            let _ = server.commit(&mut c, tx);
            return Err(e);
        }
    };
    let actor = tracking::user(ctx);
    let mut c = server.core.lock().unwrap();
    let run = c
        .run(&run_id)
        .cloned()
        .ok_or_else(|| internal("the reviewer run vanished"))?;
    let existing = tracking::bindings(&c)
        .into_iter()
        .filter(|b| b.run_id == run.id)
        .collect::<Vec<_>>();
    // Deterministic association: this run was created by this very request (its id comes from
    // the launch, not from detection), so the binding is not a guess (15 §3, 04).
    let b = binding::bind(
        &existing,
        BindRequest {
            task_id: rq.task.clone(),
            run_id: run.id.clone(),
            native_conversation_id: run.harness_session_id.clone().unwrap_or_default(),
            role: BindingRole::Review,
            start_turn: run.turns_completed + 1,
            end_turn: None,
            actor: actor.clone(),
        },
        IdentityEvidence {
            deterministic: true,
        },
        now(),
    )
    .map_err(|e| conflict("binding_conflict", e.to_string()))?;
    rq.state = ReviewerState::Started;
    rq.run = Some(run.id.clone());
    rq.pane = Some(pane);
    rq.binding = Some(b.id.clone());
    rq.started_at_ms = Some(now());
    let mut tx = Tx::new();
    tx.m.put(tracking::K_BINDING, &b.id, None, &b);
    tx.event(
        "task.binding_changed",
        json!({"task": rq.task, "run": run.id, "binding": b.id}),
        json!({"state": "active", "role": "review"}),
    );
    tx.m.put(K_REVREQ, &rq.id, None, &rq);
    tx.event_by(
        "review.reviewer_started",
        json!({"task": rq.task, "request": rq.id, "run": run.id}),
        json!({"kind": "user", "id": actor.id}),
        json!({"subject": rq.subject_id, "harness": rq.harness, "binding": b.id, "prompt_digest": rq.prompt_digest}),
    );
    server.commit(&mut c, tx).map_err(internal)?;
    drop(c);
    Ok(json!({
        "request": reviewer_json(&rq),
        "run": run.id,
        "binding": b,
        "note": "The reviewer's findings are recorded as review notes (agent opinions). Classify them; they cannot accept, waive checks or override observations.",
    }))
}

/// Hook from `on_turn_settled`: a settled turn of a run bound with role `review` becomes
/// attributed review notes (once per binding and turn).
pub(super) fn on_reviewer_turn(server: &Arc<Server>, run: &str) -> Vec<String> {
    let reviews: Vec<TaskRunBinding> = server.with_core(|c| {
        tracking::bindings(c)
            .into_iter()
            .filter(|b| {
                b.run_id == run && b.role == BindingRole::Review && b.state == BindingState::Active
            })
            .collect()
    });
    if reviews.is_empty() {
        return vec![];
    }
    let turns = tracking::turns_of(server, run, 50);
    let mut touched = Vec::new();
    for b in reviews {
        let subject = server.with_core(|c| {
            c.store
                .load_by_field::<ReviewerRequest>(K_REVREQ, "$.binding", &b.id)
                .ok()
                .and_then(|v| v.into_iter().next())
                .map(|r| r.subject_id)
        });
        let mut c = server.core.lock().unwrap();
        let existing = notes_of_c(&c, &b.task_id);
        let mut tx = Tx::new();
        for t in turns.iter().filter(|t| {
            t.ended_at_ms.is_some()
                && b.covers(t.n)
                && !existing
                    .iter()
                    .any(|n| n.binding.as_deref() == Some(b.id.as_str()) && n.turn == Some(t.n))
        }) {
            let Some(msg) = t.last_message.as_deref() else {
                continue;
            };
            let notes = reviewer::notes_from_turn(
                &b.task_id,
                subject.as_deref(),
                run,
                &b.id,
                t.n,
                msg,
                now(),
            );
            for n in &notes {
                tx.m.put(K_NOTE, &n.id, None, n);
            }
            if !notes.is_empty() {
                tx.event(
                    "review.notes_recorded",
                    json!({"task": b.task_id, "run": run, "binding": b.id}),
                    json!({"turn": t.n, "count": notes.len(), "subject": subject, "open_concerns": notes.iter().filter(|n| n.is_open_concern()).count()}),
                );
            }
        }
        if !tx.m.is_empty() {
            let _ = server.commit(&mut c, tx);
            touched.push(b.task_id.clone());
        }
    }
    for t in &touched {
        spawn_refresh(server, t);
    }
    touched
}

/// `task.review.notes {task}`.
fn notes_api(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let task = req(p, "task")?;
    authorize_task(server, ctx, task)?;
    let task = tracking::find_task(server, task)?;
    let notes = notes_of(server, &task.id);
    Ok(json!({
        "task": task.id,
        "notes": notes.iter().map(note_json).collect::<Vec<_>>(),
        "reviewer_runs": reviewer_requests(server, &task.id).iter().map(reviewer_json).collect::<Vec<_>>(),
    }))
}

/// `task.review.note.classify {note, classification: blocking|not_blocking|dismissed, reason?}`:
/// the user's attributed decision on a reviewer finding (15 §7).
fn classify_api(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    const M: &str = "task.review.note.classify";
    if let Some(r) = receipts::replay(server, ctx, M, p) {
        return r;
    }
    let id = req(p, "note")?;
    let to = Classification::parse(req(p, "classification")?)
        .ok_or_else(|| invalid("classification is blocking | not_blocking | dismissed"))?;
    let note = server
        .with_core(|c| c.store.get::<ReviewNote>(K_NOTE, id).ok().flatten())
        .ok_or_else(|| not_found("review note", id))?;
    authorize_task(server, ctx, &note.task)?;
    let n = reviewer::classify(&note, to, tracking::user(ctx), s(p, "reason"), now())
        .map_err(|e| conflict("classification_refused", e.to_string()))?;
    let result = json!({"note": note_json(&n)});
    {
        let mut c = server.core.lock().unwrap();
        if let Some(r) = receipts::replay_in(&c, ctx, M, p) {
            return r;
        }
        let mut tx = Tx::new();
        tx.m.put(K_NOTE, &n.id, None, &n);
        tx.event_by(
            "review.note_classified",
            json!({"task": n.task, "note": n.id}),
            json!({"kind": "user", "id": n.classified_by.as_ref().map(|a| a.id.clone())}),
            json!({"classification": n.classification}),
        );
        receipts::record(&mut tx, ctx, M, p, &result);
        server.commit(&mut c, tx).map_err(internal)?;
    }
    spawn_refresh(server, &n.task);
    Ok(result)
}

fn dep_error(e: dep::DependencyError) -> RpcError {
    let reason = match &e {
        dep::DependencyError::SelfDependency => "self_dependency",
        dep::DependencyError::Cycle { .. } => "dependency_cycle",
        dep::DependencyError::Duplicate { .. } => "duplicate",
        dep::DependencyError::NotUser => "not_user",
    };
    let mut d = serde_json::to_value(&e).unwrap_or(json!({}));
    d["reason"] = json!(reason);
    err(ErrorKind::Conflict, e.to_string()).details(d)
}

fn parse_kind(p: &Value) -> Result<DependencyKind, RpcError> {
    match s(p, "kind") {
        None => Ok(DependencyKind::Blocks),
        Some(k) => DependencyKind::parse(k).ok_or_else(|| invalid("kind is blocks | related")),
    }
}

/// `task.dependency.add {task, depends_on, kind?}`: confirm an explicit edge ("`task` waits for
/// `depends_on`"). Cycles of `blocks` edges are refused; the confirming user is recorded.
fn dependency_add(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    const M: &str = "task.dependency.add";
    if let Some(r) = receipts::replay(server, ctx, M, p) {
        return r;
    }
    let task = tracking::find_task(server, req(p, "task")?)?;
    let on = tracking::find_task(server, req(p, "depends_on")?)?;
    authorize_task(server, ctx, &task.id)?;
    authorize_task(server, ctx, &on.id)?;
    let kind = parse_kind(p)?;
    let mut c = server.core.lock().unwrap();
    if let Some(r) = receipts::replay_in(&c, ctx, M, p) {
        return r;
    }
    // Cycle check and insert under one lock: two concurrent adds can't close a loop together.
    let edges = dep_edges(&c);
    let e = dep::add_edge(&edges, &task.id, &on.id, kind, tracking::user(ctx), now())
        .map_err(dep_error)?;
    let result = json!({"edge": e, "note": "Confirmed link; it only informs ranking and never changes either task."});
    let mut tx = Tx::new();
    tx.m.put(K_DEP, &e.id, None, &e);
    tx.event_by(
        "task.dependency_changed",
        json!({"task": e.task, "depends_on": e.depends_on, "edge": e.id}),
        json!({"kind": "user", "id": e.confirmed_by.id}),
        json!({"action": "added", "kind": e.kind}),
    );
    receipts::record(&mut tx, ctx, M, p, &result);
    server.commit(&mut c, tx).map_err(internal)?;
    Ok(result)
}

/// `task.dependency.remove {edge} | {task, depends_on, kind?}`: the edge row is closed (kept as
/// history) with the removing user.
fn dependency_remove(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    const M: &str = "task.dependency.remove";
    if let Some(r) = receipts::replay(server, ctx, M, p) {
        return r;
    }
    let edges = server.with_core(|c| dep_edges(c));
    let found: Vec<DependencyEdge> = match s(p, "edge") {
        Some(id) => edges.into_iter().filter(|e| e.id == id).collect(),
        None => {
            let task = tracking::find_task(server, req(p, "task")?)?;
            let on = tracking::find_task(server, req(p, "depends_on")?)?;
            let kind = s(p, "kind").map(|_| parse_kind(p)).transpose()?;
            edges
                .into_iter()
                .filter(|e| {
                    e.task == task.id && e.depends_on == on.id && kind.is_none_or(|k| k == e.kind)
                })
                .collect()
        }
    };
    if found.is_empty() {
        return Err(not_found("dependency", s(p, "edge").unwrap_or("link")));
    }
    for e in &found {
        authorize_task(server, ctx, &e.task)?;
    }
    let actor = tracking::user(ctx);
    let result = json!({"removed": found, "removed_by": actor});
    let mut c = server.core.lock().unwrap();
    if let Some(r) = receipts::replay_in(&c, ctx, M, p) {
        return r;
    }
    let mut tx = Tx::new();
    for e in &found {
        tx.m.close(K_DEP, &e.id, None, e);
        tx.event_by(
            "task.dependency_changed",
            json!({"task": e.task, "depends_on": e.depends_on, "edge": e.id}),
            json!({"kind": "user", "id": actor.id}),
            json!({"action": "removed", "kind": e.kind}),
        );
    }
    receipts::record(&mut tx, ctx, M, p, &result);
    server.commit(&mut c, tx).map_err(internal)?;
    Ok(result)
}

/// `task.dependency.list {task?}`.
fn dependency_list(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    match s(p, "task") {
        Some(t) => {
            authorize_task(server, ctx, t)?;
            let task = tracking::find_task(server, t)?;
            Ok(json!({"task": task.id, "dependencies": dependencies_json(server, &task.id)}))
        }
        None => {
            if caller_workspace(server, ctx)?.is_some() {
                return Err(invalid("pane-scoped callers must name a task"));
            }
            Ok(json!({"edges": server.with_core(|c| dep_edges(c))}))
        }
    }
}

/// `task.effort.estimate {task}`: the user-set effort and the deterministic heuristic (labelled
/// `heuristic`). The model estimate is `assistant.generate effort_estimate`. Never applies.
async fn effort_api(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let task = req(p, "task")?;
    authorize_task(server, ctx, task)?;
    let pkg = package(server, task, None).await?;
    Ok(json!({
        "task": pkg.task.id,
        "effort": pkg.json["effort"],
        "model_estimate": {"method": "assistant.generate", "params": {"operation": "effort_estimate", "task": pkg.task.id}, "note": "Optional; shows the exact payload first and is applied only with task.set."},
    }))
}

// ---- test seam ----------------------------------------------------------------------------------

#[cfg(test)]
pub(crate) type Launcher =
    Arc<dyn Fn(&Arc<Server>, &ReviewerRequest) -> Result<(String, String), RpcError> + Send + Sync>;

#[cfg(test)]
fn launchers() -> &'static Mutex<HashMap<String, Launcher>> {
    static M: OnceLock<Mutex<HashMap<String, Launcher>>> = OnceLock::new();
    M.get_or_init(Default::default)
}

/// Test seam: launch the reviewer for `task` with `f` instead of a real harness.
#[cfg(test)]
pub(crate) fn set_test_launcher(task: &str, f: Launcher) {
    launchers().lock().unwrap().insert(task.to_string(), f);
}

#[cfg(test)]
fn test_launcher(task: &str) -> Option<Launcher> {
    launchers().lock().unwrap().get(task).cloned()
}
