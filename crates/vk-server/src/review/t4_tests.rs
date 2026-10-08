//! Spec 15 T4 in-process tests: validated dirty snapshots (accept, verify, invalidate, concurrent
//! writers), the effort heuristic, reviewer runs (prompt confirmation, `review` binding,
//! findings as notes) and confirmed dependency links (cycles, ranking explanation).
//! Checks run only after an explicit per-candidate authorization; no harness or model is
//! contacted (the reviewer launch goes through the test seam).

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

fn status(e: &Env) -> String {
    git(
        &e.repo,
        &["status", "--porcelain=v2", "-z", "--untracked-files=all"],
    )
}

async fn tracked(e: &Env, criteria: Value) -> String {
    e.add_run("r1", &e.repo);
    e.turn("r1", "Fix the login redirect", &[], "on it");
    track(e, "r1", criteria).await
}

#[tokio::test(flavor = "multi_thread")]
async fn dirty_snapshot_accepts_verifies_and_a_later_change_invalidates() {
    let e = Env::new();
    let task = tracked(&e, json!(["Looks right"])).await;
    // Uncommitted work only (no commit since the base): T2 would leave it inspect-only.
    e.write("status.txt", "pass\n");
    e.write("wip/new.bin", "\0\x01binary");
    let pkg = review(&e, &task).await;
    assert!(pkg["subject"].is_null());
    assert_eq!(pkg["inspect_only"][0]["kind"], "checkout_live");
    assert_eq!(pkg["snapshot"]["available"], true);

    let before = status(&e);
    let index_before = std::fs::read(e.repo.join(".git/index")).unwrap();
    let snap = ok(
        &e,
        "task.review.snapshot",
        json!({"task": task, "idempotency_key": "snap-1"}),
    )
    .await;
    let s1 = snap["subject"]["id"].as_str().unwrap().to_string();
    assert_eq!(snap["subject"]["kind"], "dirty_snapshot");
    assert!(
        snap["snapshot"]["ref_name"]
            .as_str()
            .unwrap()
            .starts_with("refs/vibeke/snapshots/")
    );
    // Idempotent per key; the user's checkout, index and branches are untouched.
    let again = ok(
        &e,
        "task.review.snapshot",
        json!({"task": task, "idempotency_key": "snap-1"}),
    )
    .await;
    assert_eq!(again["replayed"], true);
    assert_eq!(status(&e), before);
    assert_eq!(
        std::fs::read(e.repo.join(".git/index")).unwrap(),
        index_before
    );
    assert_eq!(
        git(
            &e.repo,
            &["for-each-ref", "--format=%(refname)", "refs/heads"]
        ),
        "refs/heads/main"
    );
    assert_eq!(events(&e, "review.snapshot_created").len(), 1);

    // The snapshot is now the current, accept-capable candidate.
    let pkg = review(&e, &task).await;
    assert_eq!(subject_of(&pkg), s1);
    assert_eq!(pkg["subject"]["kind"], "dirty_snapshot");
    assert_eq!(pkg["accept_capable"], true, "{}", pkg["actions"]);
    assert!(!blockers(&pkg).contains(&"subject_not_committed".to_string()));
    let files: Vec<&str> = pkg["diff_stat"]["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert!(files.contains(&"status.txt") && files.contains(&"wip/new.bin"));
    // Its diff comes from the stored snapshot, not the live tree.
    let d = ok(&e, "task.review.diff", json!({"task": task, "subject": s1})).await;
    assert!(d["diff"].as_str().unwrap().contains("+pass"));
    assert_ne!(d["content_sha"], d["head_sha"]);

    // Checks run on it in a disposable checkout materialized from the snapshot, only after
    // the per-candidate authorization. `unit` passes only with the uncommitted status.txt.
    assert_eq!(
        reason(
            call(
                &e,
                "task.check.run",
                json!({"task": task, "check": "unit", "subject": s1})
            )
            .await
        ),
        "authorization_required"
    );
    let run = verify(&e, &task, &s1, "unit").await;
    assert_eq!(run["state"], "passed", "{run}");
    assert_eq!(run["subject_id"], s1.as_str());
    assert_eq!(
        status(&e),
        before,
        "verification never touches the checkout"
    );

    // Acceptance on the dirty snapshot (extends T2's committed-only rule).
    let acc = ok(
        &e,
        "task.review.accept",
        json!({"task": task, "intent_revision": 1, "subject_id": s1}),
    )
    .await;
    assert_eq!(acc["label"], "reviewed");
    assert_eq!(acc["acceptance"]["subject_id"], s1.as_str());
    assert_eq!(review(&e, &task).await["label"], "reviewed");

    // A later edit: the snapshot is no longer current and the acceptance is outdated.
    e.write("status.txt", "pass\nmore\n");
    let pkg = review(&e, &task).await;
    assert_eq!(pkg["label"], "review_outdated");
    assert_eq!(pkg["acceptance"]["status"], "outdated");
    assert!(pkg["subject"].is_null(), "no current candidate");
    let earlier = pkg["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == s1.as_str())
        .unwrap();
    assert_eq!(earlier["current"], false);
    assert_eq!(earlier["accept_capable"], false);
    assert_eq!(events(&e, "review.invalidated").len(), 1);
    // The old snapshot can't be accepted again (there is no current candidate to accept).
    let r = call(
        &e,
        "task.review.accept",
        json!({"task": task, "intent_revision": 1, "subject_id": s1}),
    )
    .await;
    assert_eq!(reason(r), "no_subject");

    // A new snapshot; an edit between showing it and accepting it is a known competing update.
    let s2 = ok(&e, "task.review.snapshot", json!({"task": task})).await["subject"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(s2, s1);
    let repo = e.repo.clone();
    hooks::set(
        "accept_after_package",
        &task,
        Arc::new(move || std::fs::write(repo.join("status.txt"), "pass\nlast-second\n").unwrap()),
    );
    let r = call(
        &e,
        "task.review.accept",
        json!({"task": task, "intent_revision": 1, "subject_id": s2}),
    )
    .await;
    hooks::clear("accept_after_package", &task);
    assert_eq!(reason(r), "review_changed");
    assert_eq!(events(&e, "review.accepted").len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn snapshot_refuses_a_changing_or_clean_checkout() {
    let e = Env::new();
    let task = tracked(&e, json!(["Looks right"])).await;
    assert_eq!(
        reason(call(&e, "task.review.snapshot", json!({"task": task})).await),
        "nothing_to_snapshot"
    );
    // A writer changes the checkout during every capture attempt.
    let (n, repo) = (
        Arc::new(std::sync::atomic::AtomicU64::new(0)),
        e.repo.clone(),
    );
    hooks::set(
        "snapshot_capture",
        &task,
        Arc::new(move || {
            let i = n.fetch_add(1, Ordering::Relaxed);
            std::fs::write(repo.join("busy.txt"), format!("{i}\n")).unwrap();
        }),
    );
    let r = call(&e, "task.review.snapshot", json!({"task": task})).await;
    hooks::clear("snapshot_capture", &task);
    let err = r.unwrap_err();
    assert_eq!(err.data.details["reason"], "workspace_changing", "{err:?}");
    assert_eq!(
        err.data.details["label"],
        "Workspace changing — verification subject unavailable"
    );
    assert!(events(&e, "review.snapshot_created").is_empty());
    assert_eq!(
        git(&e.repo, &["for-each-ref", "refs/vibeke/snapshots/"]),
        "",
        "no snapshot ref for an inconsistent capture"
    );
    // Once quiet, a snapshot works.
    assert_eq!(
        ok(&e, "task.review.snapshot", json!({"task": task})).await["subject"]["kind"],
        "dirty_snapshot"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn effort_heuristic_is_labelled_and_never_applied() {
    let e = Env::new();
    let task = tracked(&e, json!(["Looks right"])).await;
    e.write("small.txt", "one line\n");
    e.commit("small change");
    let pkg = review(&e, &task).await;
    let h = &pkg["effort"]["heuristic"];
    assert_eq!(h["effort"], "quick", "{h}");
    assert_eq!(h["source"], "heuristic");
    assert!(pkg["effort"]["set"].is_null());
    let est = ok(&e, "task.effort.estimate", json!({"task": task})).await;
    assert_eq!(est["effort"]["heuristic"]["effort"], "quick");
    assert_eq!(
        est["model_estimate"]["params"]["operation"],
        "effort_estimate"
    );
    // Not applied to the task; the inbox shows it as an estimate with its source.
    let t = tracking::find_task(&e.server, &task).unwrap();
    assert!(t.effort.is_none());
    let v = ok(&e, "attention.list", json!({})).await;
    let item = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["key"]["kind"] == "review")
        .unwrap();
    assert_eq!(item["effort"], "unknown");
    assert_eq!(
        item["effort_estimate"],
        json!({"effort": "quick", "source": "heuristic"})
    );
    // The user applies an estimate explicitly; the event records where it came from.
    ok(
        &e,
        "task.set",
        json!({"task": task, "effort": "quick", "effort_source": "heuristic"}),
    )
    .await;
    let up = events(&e, "task.updated");
    assert_eq!(up.last().unwrap()["data"]["effort_source"], "heuristic");
    let v = ok(&e, "attention.list", json!({})).await;
    let item = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["key"]["kind"] == "review")
        .unwrap();
    assert_eq!(item["effort"], "quick");
    assert!(item["effort_estimate"].is_null());
}

#[tokio::test(flavor = "multi_thread")]
async fn reviewer_run_needs_the_exact_prompt_binds_review_and_records_notes() {
    let e = Env::new();
    let task = tracked(&e, json!(["Looks right"])).await;
    e.write("status.txt", "pass\n");
    let head = e.commit("fix");
    let pkg = review(&e, &task).await;
    let subj = subject_of(&pkg);

    let launched = Arc::new(AtomicUsize::new(0));
    let l2 = launched.clone();
    let repo = e.repo.clone();
    t4::set_test_launcher(
        &task,
        Arc::new(move |srv, _rq| {
            l2.fetch_add(1, Ordering::SeqCst);
            let t = now();
            let r = AgentRun {
                id: "rev1".into(),
                handle: "h-rev1".into(),
                name: Some("rev1".into()),
                pane: "pane-rev1".into(),
                harness: "claude".into(),
                harness_version: None,
                integration: "process".into(),
                harness_session_id: Some("sess-rev1".into()),
                transcript_path: None,
                resume_argv: vec![],
                cwd: Some(repo.to_string_lossy().into_owned()),
                model: None,
                task: None,
                execution: Facet {
                    value: Execution::Idle,
                    since_ms: t,
                    source: StateSource::Structured,
                    confidence: 1.0,
                    detail: None,
                },
                health: AdapterHealth::Healthy,
                yolo: false,
                permission_mode: None,
                last_message: None,
                last_tool: None,
                turns_completed: 0,
                done_rev: 0,
                started_at_ms: t,
                ended_at_ms: None,
                capabilities: vec![],
                usage: Default::default(),
                rate_limit: None,
            };
            let mut c = srv.core.lock().unwrap();
            let mut tx = Tx::new();
            tx.run(r);
            srv.commit(&mut c, tx).unwrap();
            Ok(("rev1".into(), "pane-rev1".into()))
        }),
    );

    // Request: a reviewable prompt, nothing launched or sent.
    let rq = ok(
        &e,
        "task.review.request_reviewer",
        json!({"task": task, "harness": "claude"}),
    )
    .await;
    let prompt = rq["prompt"].as_str().unwrap();
    assert!(prompt.contains("Review only"));
    assert!(prompt.contains(&format!(
        "git diff {} {head}",
        pkg["subject"]["base_sha"].as_str().unwrap()
    )));
    assert!(prompt.contains("Looks right"));
    assert_eq!(rq["requires_confirmation"], true);
    assert_eq!(rq["subject"], subj.as_str());
    let id = rq["request"]["id"].as_str().unwrap().to_string();
    let digest = rq["prompt_digest"].as_str().unwrap().to_string();
    assert_eq!(launched.load(Ordering::SeqCst), 0);
    let ev = events(&e, "review.reviewer_requested");
    assert_eq!(ev.len(), 1);
    assert!(
        !ev[0].to_string().contains("Review only"),
        "events carry metadata, not the prompt"
    );

    // A different (or stale) prompt is refused with nothing launched.
    assert_eq!(
        reason(
            call(
                &e,
                "task.review.start_reviewer",
                json!({"request": id, "prompt_digest": "0000"})
            )
            .await
        ),
        "prompt_mismatch"
    );
    assert_eq!(launched.load(Ordering::SeqCst), 0);
    // A user-edited prompt is a new request with its own digest.
    let edited = ok(
        &e,
        "task.review.request_reviewer",
        json!({"task": task, "prompt": "Review the redirect validation only."}),
    )
    .await;
    assert_eq!(edited["request"]["prompt_source"], "user_edited");
    assert_ne!(edited["prompt_digest"], digest.as_str());

    // Confirm the exact prompt: launched once, bound with role `review`.
    let st = ok(
        &e,
        "task.review.start_reviewer",
        json!({"request": id, "prompt_digest": digest}),
    )
    .await;
    assert_eq!(launched.load(Ordering::SeqCst), 1);
    assert_eq!(st["binding"]["role"], "review");
    assert_eq!(st["binding"]["run_id"], "rev1");
    assert_eq!(st["request"]["state"], "started");
    let replay = ok(
        &e,
        "task.review.start_reviewer",
        json!({"request": id, "prompt_digest": digest}),
    )
    .await;
    assert_eq!(replay["replayed"], true);
    assert_eq!(launched.load(Ordering::SeqCst), 1, "never launched twice");
    // The reviewer is not the task's implementation run.
    assert!(e.run("rev1").task.is_none());

    // The reviewer's turn: findings become attributed notes, never evidence.
    e.turn(
        "rev1",
        "review",
        &[("sh ci/test.sh", 0)],
        "FINDING [blocking] status.txt: the value is not validated\nFINDING [nit] wording",
    );
    let pkg = review(&e, &task).await;
    let notes = pkg["review_notes"].as_array().unwrap();
    assert_eq!(notes.len(), 2, "{notes:?}");
    assert!(notes.iter().all(|n| n["category"] == "agent_claim"
        && n["author"]["kind"] == "agent"
        && n["subject_id"] == subj.as_str()));
    let blocking = notes.iter().find(|n| n["severity"] == "blocking").unwrap();
    assert_eq!(blocking["open_concern"], true);
    assert!(
        pkg["observed_commands"].as_array().unwrap().is_empty(),
        "the reviewer's commands are not the implementation's evidence"
    );
    assert!(
        pkg["claims"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| !c["command"].as_str().unwrap_or("").contains("FINDING"))
    );
    assert!(blockers(&pkg).contains(&"blocking_concern".to_string()));
    assert_ne!(pkg["label"], "ready_for_review");
    // Recorded once per turn.
    t4::on_reviewer_turn(&e.server, "rev1");
    assert_eq!(
        review(&e, &task).await["review_notes"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    // Only the user classifies; dismissing needs a reason and is attributed.
    let nid = blocking["id"].as_str().unwrap().to_string();
    assert_eq!(
        reason(
            call(
                &e,
                "task.review.note.classify",
                json!({"note": nid, "classification": "dismissed"})
            )
            .await
        ),
        "classification_refused"
    );
    let c = ok(
        &e,
        "task.review.note.classify",
        json!({"note": nid, "classification": "dismissed", "reason": "validated upstream"}),
    )
    .await;
    assert_eq!(c["note"]["classified_by"]["kind"], "user");
    let pkg = review(&e, &task).await;
    assert!(!blockers(&pkg).contains(&"blocking_concern".to_string()));
    // A note never accepts anything.
    assert!(pkg["acceptance"].is_null());
}

#[tokio::test(flavor = "multi_thread")]
async fn dependency_links_refuse_cycles_and_explain_ranking() {
    let e = Env::new();
    let t0 = now();
    for (id, title) in [
        ("t1", "Shared auth helper"),
        ("t2", "Login redirect"),
        ("t3", "Invoice export"),
        ("t4", "Old review"),
    ] {
        put_task(&e, id, title, None);
    }
    // t2 waits for t1; t3 waits for t2.
    let a = ok(
        &e,
        "task.dependency.add",
        json!({"task": "t2", "depends_on": "t1"}),
    )
    .await;
    assert_eq!(a["edge"]["confirmed_by"]["kind"], "user");
    assert_eq!(a["edge"]["kind"], "blocks");
    ok(
        &e,
        "task.dependency.add",
        json!({"task": "t3", "depends_on": "t2"}),
    )
    .await;
    let r = call(
        &e,
        "task.dependency.add",
        json!({"task": "t1", "depends_on": "t3"}),
    )
    .await;
    let err = r.unwrap_err();
    assert_eq!(err.data.details["reason"], "dependency_cycle");
    assert_eq!(err.data.details["path"], json!(["t1", "t3", "t2", "t1"]));
    assert_eq!(
        reason(
            call(
                &e,
                "task.dependency.add",
                json!({"task": "t1", "depends_on": "t1"})
            )
            .await
        ),
        "self_dependency"
    );
    assert_eq!(
        reason(
            call(
                &e,
                "task.dependency.add",
                json!({"task": "t2", "depends_on": "t1"})
            )
            .await
        ),
        "duplicate"
    );
    let ev = events(&e, "task.dependency_changed");
    assert_eq!(ev.len(), 2);
    assert_eq!(ev[0]["tier"], "history");
    assert_eq!(ev[0]["data"]["action"], "added");

    // Ranking: t1 blocks two open tasks (t2 directly, t3 through t2).
    put_projection(&e, projection("t1", "review_available", t0 - 60_000, 1));
    put_projection(&e, projection("t4", "review_available", t0 - 3_600_000, 1));
    let v = ok(&e, "attention.list", json!({})).await;
    let reviews: Vec<&Value> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["key"]["kind"] == "review")
        .collect();
    assert_eq!(reviews[0]["task"], "t1", "{reviews:?}");
    assert_eq!(reviews[0]["blocks_tasks"], 2);
    assert!(
        reviews[0]["explanation"]
            .as_str()
            .unwrap()
            .contains("blocks 2 linked tasks")
    );
    assert!(
        !reviews[1]["explanation"]
            .as_str()
            .unwrap()
            .contains("blocks")
    );
    let l = ok(&e, "task.dependency.list", json!({"task": "t1"})).await;
    assert_eq!(l["dependencies"]["blocks_open_tasks"], 2);
    assert_eq!(l["dependencies"]["dependents"][0]["edge"]["task"], "t2");

    // A finished dependent no longer counts; removing an edge is recorded.
    let mut t3 = tracking::find_task(&e.server, "t3").unwrap();
    t3.status = "finished".into();
    {
        let mut c = e.server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.task(t3);
        e.server.commit(&mut c, tx).unwrap();
    }
    let v = ok(&e, "attention.list", json!({})).await;
    let t1 = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["task"] == "t1")
        .unwrap()
        .clone();
    assert!(
        t1["explanation"]
            .as_str()
            .unwrap()
            .contains("blocks 1 linked task")
    );
    let rm = ok(
        &e,
        "task.dependency.remove",
        json!({"task": "t2", "depends_on": "t1"}),
    )
    .await;
    assert_eq!(rm["removed"].as_array().unwrap().len(), 1);
    assert_eq!(
        events(&e, "task.dependency_changed").last().unwrap()["data"]["action"],
        "removed"
    );
    let v = ok(&e, "attention.list", json!({})).await;
    let t1 = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["task"] == "t1")
        .unwrap()
        .clone();
    assert_eq!(t1["blocks_tasks"], 0);
}

#[test]
fn t4_mutations_are_human_only_and_listed() {
    init_env();
    let dir = tempfile::tempdir().unwrap();
    let server = Server::new(
        Paths {
            session: "t".into(),
            runtime: dir.path().join("run"),
            state: dir.path().join("state"),
        },
        ServerOpts {
            session: "t".into(),
            machine: "m".into(),
            bin: "/bin/false".into(),
            hold_args: vec![],
            default_shell: None,
            env: vec![],
            shims: false,
            gateway: None,
        },
    )
    .unwrap();
    let ctx = Ctx {
        client_id: "agent".into(),
        kind: "agent".into(),
        pane_scope: Some("pane-x".into()),
        remote: false,
    };
    for (m, mutating) in t4::METHODS {
        assert!(
            crate::api::METHODS.contains(&(*m, *mutating)),
            "{m} missing from api::METHODS"
        );
        let r = crate::api::authorize(&server, &ctx, m, &json!({}));
        if *mutating {
            assert_eq!(
                r.unwrap_err().code,
                ErrorKind::PermissionDenied.code(),
                "{m}"
            );
        } else {
            assert!(r.is_ok(), "{m}");
        }
    }
}

// ---- review findings (2026-10-06) ---------------------------------------------------------------

/// A reviewer run as the launch path would create it (fresh: no completed turns).
fn reviewer_run(id: &str, cwd: &Path) -> AgentRun {
    let t = now();
    AgentRun {
        id: id.into(),
        handle: format!("h-{id}"),
        name: Some(id.into()),
        pane: format!("pane-{id}"),
        harness: "claude".into(),
        harness_version: None,
        integration: "process".into(),
        harness_session_id: Some(format!("sess-{id}")),
        transcript_path: None,
        resume_argv: vec![],
        cwd: Some(cwd.to_string_lossy().into_owned()),
        model: None,
        task: None,
        execution: Facet {
            value: Execution::Idle,
            since_ms: t,
            source: StateSource::Structured,
            confidence: 1.0,
            detail: None,
        },
        health: AdapterHealth::Healthy,
        yolo: false,
        permission_mode: None,
        last_message: None,
        last_tool: None,
        turns_completed: 0,
        done_rev: 0,
        started_at_ms: t,
        ended_at_ms: None,
        capabilities: vec![],
        usage: Default::default(),
        rate_limit: None,
    }
}

fn put_run_on(srv: &Arc<Server>, r: AgentRun) {
    let mut c = srv.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.run(r);
    srv.commit(&mut c, tx).unwrap();
}

/// One settled harness turn (prompt, Stop) reported through the hook path, like `Env::turn`.
fn settle_turn(srv: &Arc<Server>, run: &str, prompt: &str, last: &str) {
    let r = srv.with_core(|c| c.run(run).cloned()).unwrap();
    let sid = r.harness_session_id.clone().unwrap();
    tracking::observe(
        srv,
        &r,
        "UserPromptSubmit",
        &json!({"session_id": sid, "prompt": prompt}),
    );
    tracking::observe(
        srv,
        &r,
        "Stop",
        &json!({"session_id": sid, "last_assistant_message": last}),
    );
    let mut r = srv.with_core(|c| c.run(run).cloned()).unwrap();
    r.turns_completed += 1;
    r.done_rev += 1;
    put_run_on(srv, r);
}

/// A task with a committed candidate and a prepared reviewer request: `(task, request, digest)`.
async fn prepared_reviewer(e: &Env) -> (String, String, String) {
    let task = tracked(e, json!(["Looks right"])).await;
    e.write("status.txt", "pass\n");
    e.commit("fix");
    let rq = ok(
        e,
        "task.review.request_reviewer",
        json!({"task": task, "harness": "claude"}),
    )
    .await;
    (
        task,
        rq["request"]["id"].as_str().unwrap().to_string(),
        rq["prompt_digest"].as_str().unwrap().to_string(),
    )
}

fn reviewer_state(e: &Env, request: &str) -> String {
    e.server.with_core(|c| {
        let r = c
            .store
            .get::<t4::ReviewerRequest>(t4::K_REVREQ, request)
            .unwrap()
            .unwrap();
        serde_json::to_value(r.state)
            .unwrap()
            .as_str()
            .unwrap()
            .to_string()
    })
}

fn review_bindings_of(e: &Env, run: &str) -> usize {
    e.server.with_core(|c| {
        tracking::bindings(c)
            .into_iter()
            .filter(|b| b.run_id == run && b.role == BindingRole::Review)
            .count()
    })
}

/// Finding 6: two confirmations of one request race; the second is refused while the first
/// launches, the reviewer is launched exactly once, and the first result is replayable.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_confirmations_launch_the_reviewer_once() {
    let e = Env::new();
    let (task, id, digest) = prepared_reviewer(&e).await;
    let launched = Arc::new(AtomicUsize::new(0));
    let (release, gate) = std::sync::mpsc::channel::<()>();
    let gate = Arc::new(Mutex::new(gate));
    let (l2, repo) = (launched.clone(), e.repo.clone());
    t4::set_test_launcher(
        &task,
        Arc::new(move |srv, _rq| {
            let n = l2.fetch_add(1, Ordering::SeqCst) + 1;
            // Held at the launch barrier until the test lets it go.
            let _ = gate.lock().unwrap().recv_timeout(Duration::from_secs(30));
            let id = format!("rev{n}");
            put_run_on(srv, reviewer_run(&id, &repo));
            Ok((id.clone(), format!("pane-{id}")))
        }),
    );
    let p1 = json!({"request": id, "prompt_digest": digest, "idempotency_key": "start-1"});
    let srv = e.server.clone();
    let p = p1.clone();
    let first = tokio::spawn(async move {
        api(&srv, &user_ctx(), "task.review.start_reviewer", &p)
            .await
            .unwrap()
    });
    wait_until("the first launch", || launched.load(Ordering::SeqCst) == 1).await;
    assert_eq!(reviewer_state(&e, &id), "starting");
    // Another confirmation (another key) and a retry of the first (same key, nothing recorded
    // yet) are both refused while the first launches.
    for p in [
        json!({"request": id, "prompt_digest": digest, "idempotency_key": "start-2"}),
        p1.clone(),
        json!({"request": id, "prompt_digest": digest}),
    ] {
        assert_eq!(
            reason(call(&e, "task.review.start_reviewer", p).await),
            "reviewer_starting"
        );
    }
    release.send(()).unwrap();
    let r1 = first.await.unwrap().unwrap();
    assert_eq!(launched.load(Ordering::SeqCst), 1, "launched exactly once");
    assert_eq!(r1["request"]["state"], "started");
    assert_eq!(reviewer_state(&e, &id), "started");
    assert_eq!(review_bindings_of(&e, "rev1"), 1);
    assert_eq!(events(&e, "review.reviewer_started").len(), 1);
    // The same key replays the recorded result (operation receipt).
    let again = ok(&e, "task.review.start_reviewer", p1).await;
    assert_eq!(again["replayed"], true);
    assert_eq!(again["run"], r1["run"]);
    assert_eq!(again["binding"]["id"], r1["binding"]["id"]);
    let receipt = receipts::lookup(&e.server, &user_ctx(), "start-1").unwrap();
    assert_eq!(receipt.method, "task.review.start_reviewer");
    assert_eq!(receipt.result["run"], "rev1");
    // A later confirmation with another key is answered from the started request.
    let later = ok(
        &e,
        "task.review.start_reviewer",
        json!({"request": id, "prompt_digest": digest, "idempotency_key": "start-3"}),
    )
    .await;
    assert_eq!(later["replayed"], true);
    assert_eq!(later["run"], "rev1");
    assert_eq!(launched.load(Ordering::SeqCst), 1);
}

/// Finding 6: a failed launch returns the request to `prepared` deterministically (a retry is
/// another explicit confirmation); a restart's recovery after the launch but before the state
/// transaction leaves it `unknown`, never relaunched.
#[tokio::test(flavor = "multi_thread")]
async fn launch_failure_and_recovery_mid_launch_are_deterministic() {
    let e = Env::new();
    let (task, id, digest) = prepared_reviewer(&e).await;
    let launched = Arc::new(AtomicUsize::new(0));
    let fail = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let (l2, f2, repo) = (launched.clone(), fail.clone(), e.repo.clone());
    t4::set_test_launcher(
        &task,
        Arc::new(move |srv, _rq| {
            l2.fetch_add(1, Ordering::SeqCst);
            if f2.load(Ordering::SeqCst) {
                return Err(internal("harness not installed"));
            }
            put_run_on(srv, reviewer_run("rev1", &repo));
            Ok(("rev1".into(), "pane-rev1".into()))
        }),
    );
    let start = json!({"request": id, "prompt_digest": digest, "idempotency_key": "k1"});
    assert!(
        call(&e, "task.review.start_reviewer", start.clone())
            .await
            .is_err()
    );
    assert_eq!(reviewer_state(&e, &id), "prepared");
    let notes = ok(&e, "task.review.notes", json!({"task": task})).await;
    assert!(
        notes["reviewer_runs"][0]["error"]
            .as_str()
            .unwrap()
            .contains("harness not installed")
    );
    assert!(receipts::lookup(&e.server, &user_ctx(), "k1").is_none());

    // Retry: the launch succeeds, then a restart's recovery runs before the state transaction.
    fail.store(false, Ordering::SeqCst);
    let srv = e.server.clone();
    hooks::set(
        "reviewer_after_launch",
        &task,
        Arc::new(move || t4::recover(&srv)),
    );
    let r = call(&e, "task.review.start_reviewer", start.clone()).await;
    hooks::clear("reviewer_after_launch", &task);
    assert_eq!(reason(r), "reviewer_state_unknown");
    assert_eq!(launched.load(Ordering::SeqCst), 2);
    assert_eq!(reviewer_state(&e, &id), "unknown");
    assert_eq!(review_bindings_of(&e, "rev1"), 0, "nothing bound blindly");
    assert_eq!(events(&e, "review.reviewer_unknown").len(), 1);
    assert!(events(&e, "review.reviewer_started").is_empty());
    // Never relaunched: further confirmations are refused until the user requests anew.
    for p in [start, json!({"request": id, "prompt_digest": digest})] {
        assert_eq!(
            reason(call(&e, "task.review.start_reviewer", p).await),
            "reviewer_state_unknown"
        );
    }
    assert_eq!(launched.load(Ordering::SeqCst), 2);
    let notes = ok(&e, "task.review.notes", json!({"task": task})).await;
    assert_eq!(notes["reviewer_runs"][0]["state"], "unknown");

    // A request found `starting` at startup becomes `unknown` too.
    let rq2 = ok(&e, "task.review.request_reviewer", json!({"task": task})).await;
    let id2 = rq2["request"]["id"].as_str().unwrap().to_string();
    {
        let mut c = e.server.core.lock().unwrap();
        let mut r: t4::ReviewerRequest = c.store.get(t4::K_REVREQ, &id2).unwrap().unwrap();
        r.state = t4::ReviewerState::Starting;
        r.attempt = Some("st_crashed".into());
        let mut tx = Tx::new();
        tx.m.put(t4::K_REVREQ, &id2, None, &r);
        e.server.commit(&mut c, tx).unwrap();
    }
    t4::recover(&e.server);
    assert_eq!(reviewer_state(&e, &id2), "unknown");
    assert_eq!(launched.load(Ordering::SeqCst), 2);
}

/// Finding 7: a reviewer whose first turn (with a blocking finding) settles before the launch
/// returns still has that finding recorded — exactly once.
#[tokio::test(flavor = "multi_thread")]
async fn a_fast_reviewers_first_turn_is_recorded_once() {
    let e = Env::new();
    let (task, id, digest) = prepared_reviewer(&e).await;
    let repo = e.repo.clone();
    t4::set_test_launcher(
        &task,
        Arc::new(move |srv, _rq| {
            put_run_on(srv, reviewer_run("rev1", &repo));
            // Turn 1 finishes during the launch's readiness wait: no binding exists yet.
            settle_turn(
                srv,
                "rev1",
                "review",
                "FINDING [blocking] status.txt: the value is not validated",
            );
            Ok(("rev1".into(), "pane-rev1".into()))
        }),
    );
    let st = ok(
        &e,
        "task.review.start_reviewer",
        json!({"request": id, "prompt_digest": digest}),
    )
    .await;
    assert_eq!(st["binding"]["start_turn"], 1);
    let blocking = |pkg: &Value| {
        pkg["review_notes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|n| n["severity"] == "blocking" && n["turn"] == 1)
            .count()
    };
    let pkg = review(&e, &task).await;
    assert_eq!(blocking(&pkg), 1, "{}", pkg["review_notes"]);
    assert!(blockers(&pkg).contains(&"blocking_concern".to_string()));
    // Settlement hooks running again never duplicate it.
    t4::on_reviewer_turn(&e.server, "rev1");
    assert_eq!(blocking(&review(&e, &task).await), 1);
    // A later turn is recorded as well.
    settle_turn(&e.server, "rev1", "more", "FINDING [nit] wording");
    let pkg = review(&e, &task).await;
    assert_eq!(pkg["review_notes"].as_array().unwrap().len(), 2);
}

/// Finding 8: re-preparing an edited prompt keeps the subject the user was shown; if the
/// candidate moved meanwhile it is refused (`subject_changed`), nothing recorded.
#[tokio::test(flavor = "multi_thread")]
async fn an_edited_prompt_never_changes_the_confirmed_subject() {
    let e = Env::new();
    let task = tracked(&e, json!(["Looks right"])).await;
    e.write("status.txt", "pass\n");
    e.commit("fix");
    let rq = ok(&e, "task.review.request_reviewer", json!({"task": task})).await;
    let s1 = rq["subject"].as_str().unwrap().to_string();
    // Unchanged candidate: the edited text is recorded for the original subject.
    let edited = ok(
        &e,
        "task.review.request_reviewer",
        json!({"task": task, "prompt": "Review the redirect only.", "expected_subject": s1}),
    )
    .await;
    assert_eq!(edited["subject"], s1.as_str());
    assert_eq!(edited["request"]["subject"], s1.as_str());
    assert_eq!(edited["request"]["prompt_source"], "user_edited");
    let requested = events(&e, "review.reviewer_requested").len();

    // The candidate advances between preparation and the edited confirmation.
    e.write("status.txt", "pass\nmore\n");
    e.commit("more");
    let r = call(
        &e,
        "task.review.request_reviewer",
        json!({"task": task, "prompt": "Review the redirect only!", "expected_subject": s1}),
    )
    .await;
    let err = r.unwrap_err();
    assert_eq!(err.data.details["reason"], "subject_changed", "{err:?}");
    assert_eq!(err.data.details["expected"], s1.as_str());
    let s2 = err.data.details["current"].as_str().unwrap().to_string();
    assert_ne!(s2, s1);
    assert_eq!(
        events(&e, "review.reviewer_requested").len(),
        requested,
        "nothing recorded"
    );
    // A renewed request names the new subject explicitly.
    let renewed = ok(&e, "task.review.request_reviewer", json!({"task": task})).await;
    assert_eq!(renewed["subject"], s2.as_str());
}

/// Finding 9: an executable-bit change of an untracked file invalidates a snapshot, also
/// between showing it and accepting it.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn an_untracked_mode_change_invalidates_the_snapshot() {
    use std::os::unix::fs::PermissionsExt;
    let e = Env::new();
    let task = tracked(&e, json!(["Looks right"])).await;
    e.write("tool.sh", "#!/bin/sh\necho hi\n");
    let chmod = |repo: &Path, mode: u32| {
        std::fs::set_permissions(repo.join("tool.sh"), std::fs::Permissions::from_mode(mode))
            .unwrap()
    };
    chmod(&e.repo, 0o644);
    let s1 = ok(&e, "task.review.snapshot", json!({"task": task})).await["subject"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(subject_of(&review(&e, &task).await), s1);
    chmod(&e.repo, 0o755);
    let pkg = review(&e, &task).await;
    assert!(
        pkg["subject"].is_null(),
        "the snapshot is no longer current"
    );
    let s2 = ok(&e, "task.review.snapshot", json!({"task": task})).await["subject"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(s2, s1);
    // chmod between presentation and acceptance: refused.
    let repo = e.repo.clone();
    hooks::set(
        "accept_after_package",
        &task,
        Arc::new(move || chmod(&repo, 0o644)),
    );
    let r = call(
        &e,
        "task.review.accept",
        json!({"task": task, "intent_revision": 1, "subject_id": s2}),
    )
    .await;
    hooks::clear("accept_after_package", &task);
    assert_eq!(reason(r), "review_changed");
    assert!(events(&e, "review.accepted").is_empty());
}

/// Finding 3: a submodule with changes of its own is refused with `unsupported_capture`, and a
/// change inside a submodule invalidates an earlier snapshot.
#[tokio::test(flavor = "multi_thread")]
async fn submodule_changes_are_refused_and_invalidate_snapshots() {
    let e = Env::new();
    let lib = tempfile::tempdir().unwrap();
    let lib_path = lib.path().canonicalize().unwrap();
    git(&lib_path, &["init", "-q", "-b", "main"]);
    std::fs::write(lib_path.join("x.txt"), "lib v1\n").unwrap();
    git(&lib_path, &["add", "-A"]);
    git(&lib_path, &["commit", "-q", "-m", "lib"]);
    git(
        &e.repo,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            lib_path.to_str().unwrap(),
            "sub",
        ],
    );
    e.commit("add submodule");
    let task = tracked(&e, json!(["Looks right"])).await;
    e.write("status.txt", "pass\n");
    e.write("sub/x.txt", "lib edit\n");
    let err = call(&e, "task.review.snapshot", json!({"task": task}))
        .await
        .unwrap_err();
    assert_eq!(err.data.details["reason"], "unsupported_capture", "{err:?}");
    assert_eq!(err.data.details["unsupported"], "submodule");
    assert_eq!(err.data.details["paths"], json!(["sub"]));
    assert!(err.message.starts_with("unsupported_capture: submodule"));
    assert!(events(&e, "review.snapshot_created").is_empty());
    assert_eq!(
        git(&e.repo, &["for-each-ref", "refs/vibeke/snapshots/"]),
        ""
    );

    // Submodule clean again: the snapshot works and is current…
    git(&e.repo.join("sub"), &["checkout", "-q", "--", "x.txt"]);
    let s1 = ok(&e, "task.review.snapshot", json!({"task": task})).await["subject"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(subject_of(&review(&e, &task).await), s1);
    // …until something changes inside the submodule.
    e.write("sub/x.txt", "lib edit 2\n");
    assert!(review(&e, &task).await["subject"].is_null());
}

/// Finding 10: dependency reads show linked tasks outside the caller's workspace only as
/// placeholders, in both directions, via `task.dependency.list` and `task.review.get`.
#[tokio::test(flavor = "multi_thread")]
async fn dependency_reads_hide_linked_tasks_outside_the_callers_workspace() {
    let e = Env::new();
    put_pane(&e, "pane-a", "wsA");
    put_pane(&e, "pane-b", "wsB");
    e.add_run("a", &e.repo);
    e.add_run("b", &e.repo);
    e.turn("a", "Visible A work", &[], "");
    let ta = track(&e, "a", json!(["A ok"])).await;
    e.turn("b", "Secret B title", &[], "");
    let tb = track(&e, "b", json!(["B ok"])).await;
    let mut tc = put_task(&e, "task-secret-c", "Secret C title", None);
    tc.workspace = Some("wsB".into());
    let mut td = put_task(&e, "task-visible-d", "Visible D", None);
    td.workspace = Some("wsA".into());
    {
        let mut c = e.server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.task(tc);
        tx.task(td);
        e.server.commit(&mut c, tx).unwrap();
    }
    ok(
        &e,
        "task.dependency.add",
        json!({"task": ta, "depends_on": tb}),
    )
    .await;
    ok(
        &e,
        "task.dependency.add",
        json!({"task": "task-secret-c", "depends_on": ta}),
    )
    .await;
    ok(
        &e,
        "task.dependency.add",
        json!({"task": "task-visible-d", "depends_on": ta, "kind": "related"}),
    )
    .await;
    let hidden =
        |kind: &str| json!({"hidden": true, "title": "hidden task", "edge": {"kind": kind}});
    let leaks = |v: &Value| {
        let s = v.to_string();
        s.contains("Secret") || s.contains(&tb) || s.contains("task-secret-c")
    };

    // Pane in wsA reading its own task: the wsB endpoints are placeholders either way.
    let pa = pane_ctx("pane-a");
    let l = call_as(&e, &pa, "task.dependency.list", json!({"task": ta}))
        .await
        .unwrap();
    let d = &l["dependencies"];
    assert_eq!(d["depends_on"], json!([hidden("blocks")]), "{d}");
    let dependents = d["dependents"].as_array().unwrap();
    assert_eq!(dependents.len(), 2);
    assert!(dependents.contains(&hidden("blocks")), "{d}");
    let visible = dependents.iter().find(|x| x["hidden"].is_null()).unwrap();
    assert_eq!(visible["title"], "Visible D");
    assert_eq!(visible["edge"]["task"], "task-visible-d");
    assert!(!leaks(&l), "{l}");
    let pkg = call_as(&e, &pa, "task.review.get", json!({"task": ta}))
        .await
        .unwrap();
    assert_eq!(pkg["dependencies"], l["dependencies"]);
    assert!(!leaks(&pkg["dependencies"]), "{}", pkg["dependencies"]);

    // Pane in wsB reading its task: the wsA dependent is a placeholder.
    let pb = pane_ctx("pane-b");
    let l = call_as(&e, &pb, "task.dependency.list", json!({"task": tb}))
        .await
        .unwrap();
    assert_eq!(l["dependencies"]["dependents"], json!([hidden("blocks")]));
    assert!(!l.to_string().contains(&ta) && !l.to_string().contains("Visible A"));
    let pkg = call_as(&e, &pb, "task.review.get", json!({"task": tb}))
        .await
        .unwrap();
    assert_eq!(pkg["dependencies"]["dependents"], json!([hidden("blocks")]));

    // Full scope sees everything.
    let full = ok(&e, "task.dependency.list", json!({"task": ta})).await;
    assert_eq!(
        full["dependencies"]["depends_on"][0]["edge"]["depends_on"],
        tb.as_str()
    );
    assert!(full.to_string().contains("Secret C title"));
    let pkg = review(&e, &ta).await;
    assert!(pkg["dependencies"].to_string().contains("Secret C title"));
}

fn snapshot_refs(e: &Env) -> Vec<String> {
    let out = git(
        &e.repo,
        &[
            "for-each-ref",
            "--format=%(objectname)",
            "refs/vibeke/snapshots/",
        ],
    );
    out.lines().map(str::to_string).collect()
}

async fn snap(e: &Env, task: &str) -> (String, String) {
    let v = ok(e, "task.review.snapshot", json!({"task": task})).await;
    (
        v["subject"]["id"].as_str().unwrap().to_string(),
        v["snapshot"]["commit"].as_str().unwrap().to_string(),
    )
}

/// Snapshot refs: superseded, unaccepted snapshots lose their ref; accepted ones keep it;
/// `task.review.snapshot.gc` removes what a finished task no longer references and leaves
/// unrecorded refs unless asked.
#[tokio::test(flavor = "multi_thread")]
async fn snapshot_refs_are_collected_when_nothing_references_them() {
    let e = Env::new();
    let task = tracked(&e, json!(["Looks right"])).await;
    e.write("status.txt", "pass\n");
    let (s1, c1) = snap(&e, &task).await;
    let run = verify(&e, &task, &s1, "unit").await;
    assert_eq!(run["state"], "passed");
    ok(
        &e,
        "task.review.accept",
        json!({"task": task, "intent_revision": 1, "subject_id": s1}),
    )
    .await;
    e.write("wip.txt", "two\n");
    let (_s2, c2) = snap(&e, &task).await;
    assert_eq!(snapshot_refs(&e).len(), 2);
    e.write("wip.txt", "three\n");
    let (_s3, c3) = snap(&e, &task).await;
    // c2 was superseded and never accepted: its ref is gone; c1 (accepted) and c3 (current).
    let refs = snapshot_refs(&e);
    assert!(
        refs.contains(&c1) && refs.contains(&c3) && !refs.contains(&c2),
        "{refs:?}"
    );
    // The superseded snapshot's commit object is still there for now.
    assert_eq!(git(&e.repo, &["cat-file", "-t", &c2]), "commit");

    // A ref no record names (another session's).
    let head = git(&e.repo, &["rev-parse", "HEAD"]);
    git(
        &e.repo,
        &[
            "update-ref",
            &format!("refs/vibeke/snapshots/{head}"),
            &head,
        ],
    );
    let gc = ok(&e, "task.review.snapshot.gc", json!({"task": task})).await;
    assert!(gc["removed"].as_array().unwrap().is_empty(), "{gc}");
    assert_eq!(snapshot_refs(&e).len(), 3);

    // The task finishes: its last snapshot is no longer a candidate.
    let mut t = tracking::find_task(&e.server, &task).unwrap();
    t.status = "finished".into();
    {
        let mut c = e.server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.task(t);
        e.server.commit(&mut c, tx).unwrap();
    }
    let repo = e.repo.to_string_lossy().into_owned();
    let dry = ok(
        &e,
        "task.review.snapshot.gc",
        json!({"repo": repo, "dry_run": true}),
    )
    .await;
    assert_eq!(dry["removed"][0]["commit"], c3.as_str());
    assert_eq!(snapshot_refs(&e).len(), 3, "dry run removes nothing");
    let gc = ok(&e, "task.review.snapshot.gc", json!({"repo": repo})).await;
    let removed: Vec<&str> = gc["removed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["commit"].as_str().unwrap())
        .collect();
    assert_eq!(removed, [c3.as_str()]);
    assert_eq!(gc["unrecorded"][0]["commit"], head.as_str());
    let refs = snapshot_refs(&e);
    assert!(
        refs.contains(&c1) && refs.contains(&head) && refs.len() == 2,
        "{refs:?}"
    );
    assert_eq!(events(&e, "review.snapshot_refs_removed").len(), 1);
    // Unrecorded refs only on request; the accepted snapshot's ref stays.
    let gc = ok(
        &e,
        "task.review.snapshot.gc",
        json!({"repo": repo, "include_unrecorded": true}),
    )
    .await;
    assert_eq!(gc["removed"][0]["commit"], head.as_str());
    assert_eq!(snapshot_refs(&e), vec![c1]);
    // Branches untouched.
    assert_eq!(
        git(
            &e.repo,
            &["for-each-ref", "--format=%(refname)", "refs/heads"]
        ),
        "refs/heads/main"
    );
    // Pane scope may not collect.
    put_pane(&e, "pane-a", "wsA");
    let r = call_as(
        &e,
        &pane_ctx("pane-a"),
        "task.review.snapshot.gc",
        json!({"repo": repo}),
    )
    .await;
    assert_eq!(r.unwrap_err().code, ErrorKind::PermissionDenied.code());
}
