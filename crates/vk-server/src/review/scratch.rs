//! Lane 2C, spec 15 §6.1: **disposable reviewer checkouts**.
//!
//! `task.review.start_reviewer` runs the reviewer in a detached checkout of the reviewed
//! subject's content commit (`<state>/reviewers/<request>`, `vk_review::scratch`) instead of the
//! task's checkout, when it splits a new pane for it (the default). The reviewer is then not a
//! known writer in the task's checkout (readiness is not held back while it reviews) and
//! nothing it edits reaches the user's work. `checkout: "task"` keeps the old behaviour; a
//! reviewer started in a pane the user names (`pane`) works in that pane's directory, so
//! `checkout: "disposable"` together with `pane` is refused.
//!
//! The checkout is removed once the reviewer run has ended (or its start failed / became
//! unknown without a run): by the attention watcher, at server start, and after each settled
//! reviewer turn. Removal only touches that directory and its own worktree metadata.

use super::t4::{K_REVREQ, ReviewerRequest, ReviewerState};
use super::*;
use vk_review::scratch::{self as vscratch, ScratchMethod};

/// Where a reviewer works.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewerCheckout {
    pub path: String,
    pub method: ScratchMethod,
    pub repo: String,
    pub content_sha: String,
    pub created_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removed_at_ms: Option<i64>,
}

fn root(server: &Server) -> PathBuf {
    server.paths.state.join("reviewers")
}

fn prepared() -> &'static Mutex<HashMap<String, ReviewerCheckout>> {
    static P: OnceLock<Mutex<HashMap<String, ReviewerCheckout>>> = OnceLock::new();
    P.get_or_init(Default::default)
}

/// Before the reviewer launches: create its disposable checkout unless the caller chose the
/// task checkout or named an existing pane. Returns the directory to start the reviewer in.
pub(super) async fn prepare(
    server: &Arc<Server>,
    rq: &ReviewerRequest,
    p: &Value,
) -> Result<Option<PathBuf>, RpcError> {
    let mode = s(p, "checkout");
    let named_pane = s(p, "pane").is_some();
    match (mode, named_pane) {
        (Some("task"), _) => return Ok(None),
        (Some("disposable"), true) => {
            return Err(invalid(
                "a disposable reviewer checkout needs a new split: omit `pane` (or pass checkout: task)",
            ));
        }
        (None, true) => return Ok(None),
        (Some(m), _) if m != "disposable" => {
            return Err(invalid("checkout: disposable | task"));
        }
        _ => {}
    }
    let subj = server
        .with_core(|c| {
            c.store
                .get::<ChangeSubject>(K_SUBJECT, &rq.subject_id)
                .ok()
                .flatten()
        })
        .filter(|s| s.is_immutable());
    let Some(subj) = subj else {
        // Only an explicit request for a disposable checkout fails; the default falls back to
        // the task's checkout (with its known-writer limitation).
        if mode == Some("disposable") {
            return Err(conflict(
                "subject_unavailable",
                "the reviewed revision is not an immutable subject; a disposable checkout needs one (or pass checkout: task)",
            ));
        }
        return Ok(None);
    };
    let repo = PathBuf::from(&subj.repo.root);
    let sha = subj.content_sha().to_string();
    let dir_root = root(server);
    let id = rq.id.clone();
    let made = blocking(move || vscratch::create(&repo, &sha, &dir_root, &id)).await?;
    let (path, method) = match made {
        Ok(x) => x,
        Err(e) if mode == Some("disposable") => {
            return Err(conflict("reviewer_checkout_failed", e.to_string()).details(
                json!({"reason": "reviewer_checkout_failed", "offers": ["checkout: task"]}),
            ));
        }
        Err(e) => {
            tracing::warn!(error = %e, "reviewer checkout failed; using the task checkout");
            return Ok(None);
        }
    };
    let co = ReviewerCheckout {
        path: path.to_string_lossy().into_owned(),
        method,
        repo: subj.repo.root.clone(),
        content_sha: subj.content_sha().to_string(),
        created_at_ms: now(),
        removed_at_ms: None,
    };
    prepared().lock().unwrap().insert(rq.id.clone(), co);
    Ok(Some(path))
}

/// The checkout prepared for this request (recorded with the started request).
pub(super) fn take_prepared(request: &str) -> Option<ReviewerCheckout> {
    prepared().lock().unwrap().remove(request)
}

/// The launch failed: remove the checkout that was prepared for it.
pub(super) fn discard(server: &Server, request: &str) {
    if let Some(co) = take_prepared(request) {
        let _ = vscratch::remove(
            Path::new(&co.repo),
            &root(server),
            Path::new(&co.path),
            co.method,
        );
    }
}

/// Remove the checkouts of reviewers that are done. Blocking (git). Returns the removed
/// request ids.
pub fn sweep(server: &Server) -> Vec<String> {
    let done: Vec<ReviewerRequest> = server.with_core(|c| {
        c.store
            .load::<ReviewerRequest>(K_REVREQ)
            .unwrap_or_default()
            .into_iter()
            .chain(
                c.store
                    .load_closed::<ReviewerRequest>(K_REVREQ, 1000)
                    .unwrap_or_default(),
            )
            .filter(|r| {
                r.checkout
                    .as_ref()
                    .is_some_and(|x| x.removed_at_ms.is_none())
            })
            .filter(|r| match r.state {
                ReviewerState::Failed => true,
                ReviewerState::Unknown | ReviewerState::Started => match &r.run {
                    Some(run) => c.run(run).is_none_or(|x| x.ended_at_ms.is_some()),
                    None => r.state == ReviewerState::Unknown,
                },
                _ => false,
            })
            .collect()
    });
    let mut removed = vec![];
    for mut r in done {
        let Some(mut co) = r.checkout.clone() else {
            continue;
        };
        if !vscratch::remove(
            Path::new(&co.repo),
            &root(server),
            Path::new(&co.path),
            co.method,
        ) {
            continue;
        }
        co.removed_at_ms = Some(now());
        let mut c = server.core.lock().unwrap();
        // Re-read under the lock: never overwrite a newer state.
        if let Some(cur) = c
            .store
            .get::<ReviewerRequest>(K_REVREQ, &r.id)
            .ok()
            .flatten()
        {
            r = cur;
        }
        r.checkout = Some(co);
        let mut tx = Tx::new();
        tx.m.put(K_REVREQ, &r.id, None, &r);
        tx.event(
            "review.reviewer_checkout_removed",
            json!({"task": r.task, "request": r.id}),
            json!({"run": r.run}),
        );
        let _ = server.commit(&mut c, tx);
        removed.push(r.id.clone());
    }
    removed
}

/// At server start (prepared-but-unrecorded checkouts of a crashed launch are orphans: the
/// directory under `<state>/reviewers` with no request naming it is removed too).
pub(super) fn recover(server: &Arc<Server>) {
    let srv = server.clone();
    let _ = std::thread::Builder::new()
        .name("vk-review-scratch".into())
        .spawn(move || {
            sweep(&srv);
            let named: HashSet<String> = srv.with_core(|c| {
                c.store
                    .load::<ReviewerRequest>(K_REVREQ)
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|r| r.checkout.map(|x| x.path))
                    .collect()
            });
            let dir = root(&srv);
            if let Ok(rd) = std::fs::read_dir(&dir) {
                for e in rd.flatten() {
                    let p = e.path();
                    if !named.contains(&p.to_string_lossy().into_owned()) {
                        let _ = std::fs::remove_dir_all(&p);
                    }
                }
            }
        });
}
