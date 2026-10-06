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
    // A writer keeps changing the checkout during the capture.
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (stop2, repo) = (stop.clone(), e.repo.clone());
    let writer = std::thread::spawn(move || {
        let mut i = 0u64;
        while !stop2.load(Ordering::Relaxed) {
            i += 1;
            std::fs::write(repo.join("busy.txt"), format!("{i}\n")).unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    std::thread::sleep(Duration::from_millis(20));
    let r = call(&e, "task.review.snapshot", json!({"task": task})).await;
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
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
