//! Lane 2C (spec 15 extras) in-process tests: selected-patch snapshots, dirty end candidates,
//! human review recording, `forget` of derived objects, native deadlines, server-side batches,
//! the Also working footer, inbox notifications, the Link run status and disposable reviewer
//! checkouts. Fakes only: no harness, model or remote host is contacted.

use super::*;

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

// ---- selected-patch snapshots (§5) --------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn selected_patch_is_current_accept_capable_and_verified_alone() {
    let e = Env::new();
    let task = tracked(&e, json!([{"text": "Tests pass", "checks": ["unit"]}])).await;
    e.write("status.txt", "pass\n");
    e.write("other.txt", "unrelated work in progress\n");
    let before = status(&e);
    let snap = ok(
        &e,
        "task.review.snapshot",
        json!({"task": task, "paths": ["status.txt"], "idempotency_key": "sel-1"}),
    )
    .await;
    let sid = snap["subject"]["id"].as_str().unwrap().to_string();
    assert_eq!(snap["subject"]["kind"], "selected_patch");
    assert_eq!(snap["selection"]["paths"], json!(["status.txt"]));
    assert_eq!(snap["selection"]["excludes_other_changes"], true);
    assert!(
        snap["label"]
            .as_str()
            .unwrap()
            .contains("not part of this review")
    );
    assert_eq!(status(&e), before, "the checkout is untouched");
    assert_eq!(events(&e, "review.snapshot_created").len(), 1);

    // It is the current, accept-capable candidate; the warning says what is (not) reviewed.
    let pkg = review(&e, &task).await;
    assert_eq!(subject_of(&pkg), sid);
    assert_eq!(pkg["accept_capable"], true, "{}", pkg["actions"]);
    let warnings = pkg["warnings"].to_string();
    assert!(warnings.contains("Selected changes"), "{warnings}");
    let files: Vec<&str> = pkg["diff_stat"]["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        files,
        vec!["status.txt"],
        "only the selection is under review"
    );

    // A check against it runs in a disposable checkout of the selection alone.
    let run = verify(&e, &task, &sid, "unit").await;
    assert_eq!(run["state"], "passed", "{run}");
    assert_eq!(run["subject_id"], sid.as_str());

    // Editing an unselected file keeps it current; editing a selected one does not.
    e.write("other.txt", "more unrelated work\n");
    assert_eq!(subject_of(&review(&e, &task).await), sid);
    e.write("status.txt", "pass\nchanged\n");
    let pkg = review(&e, &task).await;
    assert!(pkg["subject"].is_null() || pkg["subject"]["id"] != sid.as_str());
}

#[tokio::test(flavor = "multi_thread")]
async fn selected_patch_from_hunks_and_refusals() {
    let e = Env::new();
    let task = tracked(&e, json!(["Looks right"])).await;
    e.write("status.txt", "pass\n");
    e.write("b.txt", "b\n");
    let patch = git(&e.repo, &["diff", "--", "status.txt"]);
    let snap = ok(
        &e,
        "task.review.snapshot",
        json!({"task": task, "patch": format!("{patch}\n")}),
    )
    .await;
    assert_eq!(snap["subject"]["kind"], "selected_patch");
    assert_eq!(snap["selection"]["mode"], "patch");
    // A selection with nothing in it, a path outside the repo, both params: refused.
    assert_eq!(
        reason(
            call(
                &e,
                "task.review.snapshot",
                json!({"task": task, "paths": ["ci/test.sh"]})
            )
            .await
        ),
        "nothing_selected"
    );
    assert_eq!(
        reason(
            call(
                &e,
                "task.review.snapshot",
                json!({"task": task, "paths": ["../x"]})
            )
            .await
        ),
        "invalid_selection"
    );
    assert!(
        call(
            &e,
            "task.review.snapshot",
            json!({"task": task, "paths": ["status.txt"], "patch": patch})
        )
        .await
        .is_err()
    );
    // A full dirty snapshot taken afterwards is newer and becomes current.
    let full = ok(&e, "task.review.snapshot", json!({"task": task})).await;
    assert_eq!(full["subject"]["kind"], "dirty_snapshot");
    let pkg = review(&e, &task).await;
    assert_eq!(pkg["subject"]["kind"], "dirty_snapshot");
}

// ---- dirty end candidates (§4.2) ----------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn closing_a_binding_pins_uncommitted_work_as_its_end_candidate() {
    let e = Env::new();
    let task = tracked(&e, json!(["Looks right"])).await;
    e.write("status.txt", "pass\n");
    ok(&e, "task.unbind", json!({"task": task})).await;
    let end: Vec<EndRec> = e.server.with_core(|c| by_task(c, K_END, &task));
    assert_eq!(end.len(), 1);
    let sid = end[0].subject_id.clone().expect("a dirty end candidate");
    assert!(end[0].note.as_deref().unwrap().contains("snapshot"));
    // Later edits (another writer) are not absorbed: the end candidate stays the snapshot.
    e.write("status.txt", "someone else's edit\n");
    let pkg = review(&e, &task).await;
    assert_eq!(subject_of(&pkg), sid);
    assert_eq!(pkg["subject"]["kind"], "dirty_snapshot");
    assert!(pkg["no_end_candidate"].as_array().unwrap().is_empty());
    // Its ref is kept by snapshot GC (pinned).
    let gc = ok(
        &e,
        "task.review.snapshot.gc",
        json!({"task": task, "dry_run": true}),
    )
    .await;
    assert!(gc["removed"].as_array().unwrap().is_empty(), "{gc}");
}

// ---- human review (§6.4) ------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn human_review_decides_a_human_criterion_on_one_subject() {
    let e = Env::new();
    let task = tracked(
        &e,
        json!(["Looks right", {"text": "Tests pass", "checks": ["unit"]}]),
    )
    .await;
    e.write("status.txt", "pass\n");
    e.commit("fix");
    let pkg = review(&e, &task).await;
    let sid = subject_of(&pkg);
    assert_eq!(criterion(&pkg, "Looks right")["status"], "needs_judgment");
    let human_id = pkg["intent"]["criteria"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let check_id = pkg["intent"]["criteria"][1]["id"]
        .as_str()
        .unwrap()
        .to_string();

    // A failed review needs a note; a check criterion can't be decided by a human review.
    assert!(
        call(
            &e,
            "task.review.human_review",
            json!({"task": task, "criterion": human_id, "verdict": "failed"})
        )
        .await
        .is_err()
    );
    assert_eq!(
        reason(
            call(
                &e,
                "task.review.human_review",
                json!({"task": task, "criterion": check_id, "verdict": "supported"})
            )
            .await
        ),
        "not_a_human_criterion"
    );
    // An unknown screenshot can't support it.
    assert!(
        call(
            &e,
            "task.review.human_review",
            json!({"task": task, "criterion": human_id, "verdict": "supported", "screenshots": ["nope"]})
        )
        .await
        .is_err()
    );
    // A pane token can't record one.
    assert!(
        call_as(
            &e,
            &pane_ctx("pane-r1"),
            "task.review.human_review",
            json!({"task": task, "criterion": human_id, "verdict": "supported"})
        )
        .await
        .is_err()
    );
    assert_eq!(
        crate::api::pane_scope_of("task.review.human_review"),
        crate::api::PaneScope::Forbidden
    );

    let r = ok(
        &e,
        "task.review.human_review",
        json!({"task": task, "criterion": human_id, "verdict": "supported", "note": "Redirect works", "expected_subject": sid, "idempotency_key": "hr-1"}),
    )
    .await;
    assert_eq!(r["review"]["subject_id"], sid.as_str());
    assert_eq!(events(&e, "review.human_reviewed").len(), 1);
    let pkg = review(&e, &task).await;
    assert_eq!(criterion(&pkg, "Looks right")["status"], "supported");
    assert_eq!(
        criterion(&pkg, "Tests pass")["status"],
        "missing",
        "never a check result"
    );
    assert_eq!(pkg["human_reviews"].as_array().unwrap().len(), 1);

    // A new revision: the review stays history for the old subject only.
    e.write("status.txt", "pass\nmore\n");
    e.commit("more");
    let pkg = review(&e, &task).await;
    assert_ne!(subject_of(&pkg), sid);
    assert_eq!(criterion(&pkg, "Looks right")["status"], "needs_judgment");

    // Withdrawn on the current subject → needs judgment again.
    let sid2 = subject_of(&pkg);
    ok(
        &e,
        "task.review.human_review",
        json!({"task": task, "criterion": human_id, "verdict": "supported"}),
    )
    .await;
    assert_eq!(
        criterion(&review(&e, &task).await, "Looks right")["status"],
        "supported"
    );
    tokio::time::sleep(Duration::from_millis(5)).await;
    ok(
        &e,
        "task.review.human_review",
        json!({"task": task, "criterion": human_id, "verdict": "withdrawn", "expected_subject": sid2}),
    )
    .await;
    assert_eq!(
        criterion(&review(&e, &task).await, "Looks right")["status"],
        "needs_judgment"
    );
}

// ---- forget (§11) -------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn forget_purges_derived_objects_and_purged_evidence_is_unknown() {
    let e = Env::new();
    let task = tracked(&e, json!([{"text": "Tests pass", "checks": ["unit"]}])).await;
    e.write("status.txt", "pass\n");
    e.commit("fix");
    let pkg = review(&e, &task).await;
    let sid = subject_of(&pkg);
    let run = verify(&e, &task, &sid, "unit").await;
    assert_eq!(run["state"], "passed");
    assert_eq!(
        criterion(&review(&e, &task).await, "Tests pass")["status"],
        "supported"
    );

    // Dry run counts and changes nothing.
    let dry = ok(
        &e,
        "task.review.forget",
        json!({"task": task, "dry_run": true}),
    )
    .await;
    assert!(dry["purged"]["turns"].as_u64().unwrap() >= 1, "{dry}");
    assert!(
        dry["purged"]["intent_excerpts"].as_u64().unwrap() >= 1,
        "{dry}"
    );
    assert_eq!(dry["purged"]["check_logs"], 1);
    assert!(events(&e, "review.purged").is_empty());
    assert_eq!(
        criterion(&review(&e, &task).await, "Tests pass")["status"],
        "supported"
    );

    let done = ok(&e, "task.review.forget", json!({"task": task})).await;
    assert_eq!(done["purged"]["check_logs"], 1);
    assert_eq!(events(&e, "review.purged").len(), 1);
    // Events carry counts, never text.
    assert!(
        !events(&e, "review.purged")[0]
            .to_string()
            .contains("Fix the login redirect")
    );
    // Excerpt and turn prompt are gone and not resurrected.
    let intent = tracking::intent_at(&e.server, &task, 1).unwrap();
    assert!(intent.source_excerpt.is_none());
    assert!(
        tracking::turns_of(&e.server, "r1", 10)
            .iter()
            .all(|t| t.prompt.is_empty())
    );
    // The check log is gone; the evidence it was is now unknown, so the criterion is not
    // supported any more.
    let pkg = review(&e, &task).await;
    assert_ne!(criterion(&pkg, "Tests pass")["status"], "supported");
    assert!(
        pkg["purged"]["counts"]["check_log"].as_u64() == Some(1),
        "{}",
        pkg["purged"]
    );
    // Pane tokens can't forget.
    assert!(
        call_as(
            &e,
            &pane_ctx("pane-r1"),
            "task.review.forget",
            json!({"task": task})
        )
        .await
        .is_err()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn scrollback_forget_scope_purges_review_objects_of_its_panes() {
    let e = Env::new();
    let task = tracked(&e, json!(["Looks right"])).await;
    let scope = json!({"pane": "pane-r1"});
    let rep = crate::review::purge::on_scrollback_forget(
        &e.server,
        &scope,
        Some(&["pane-r1".to_string()]),
        false,
    );
    assert!(rep["turns"].as_u64().unwrap() >= 1, "{rep}");
    assert_eq!(rep["tasks"], 1);
    let intent = tracking::intent_at(&e.server, &task, 1).unwrap();
    assert!(intent.source_excerpt.is_none());
}

// ---- attention: deadlines, batches, also working, notifications (§8) ----------------------------

fn put_approval(e: &Env, id: &str, run: &str, cmd: &str, opened: i64) {
    put_interaction(e, id, run, opened);
    let mut c = e.server.core.lock().unwrap();
    let mut it = c.interaction(id).cloned().unwrap();
    it.kind = InteractionKind::Approval;
    it.title = format!("Run {cmd}?");
    it.action = Some(ActionInfo {
        tool: "Bash".into(),
        summary: cmd.into(),
        command: Some(cmd.into()),
        paths: vec![],
        diff: None,
        risk: Risk::Low,
        risk_reasons: vec![],
    });
    let mut tx = Tx::new();
    tx.interaction(it);
    e.server.commit(&mut c, tx).unwrap();
}

fn item<'a>(v: &'a Value, kind: &str, id: &str) -> Option<&'a Value> {
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["key"]["kind"] == kind && i["key"]["id"] == id)
}

#[tokio::test(flavor = "multi_thread")]
async fn equivalent_approvals_form_a_batch_answered_one_by_one() {
    let e = Env::new();
    e.add_run("a1", &e.repo);
    e.add_run("a2", &e.repo);
    e.add_run("a3", &e.repo);
    let t = now() - 60_000;
    put_approval(&e, "i1", "a1", "cargo test", t);
    put_approval(&e, "i2", "a2", "cargo test", t + 1);
    put_approval(&e, "i3", "a3", "cargo test && rm -rf x", t + 2);
    let v = ok(&e, "attention.list", json!({})).await;
    let b1 = &item(&v, "interaction", "i1").unwrap()["batch"];
    let b2 = &item(&v, "interaction", "i2").unwrap()["batch"];
    assert_eq!(b1["size"], 2, "{v}");
    assert_eq!(b1["id"], b2["id"]);
    assert!(
        item(&v, "interaction", "i3").unwrap()["batch"].is_null(),
        "compound command"
    );
    assert_eq!(v["batches"].as_array().unwrap().len(), 1);

    let b = ok(&e, "attention.batch", json!({"interaction": "i2"})).await;
    assert_eq!(b["batchable"], true);
    let members: Vec<&str> = b["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["interaction"].as_str().unwrap())
        .collect();
    assert_eq!(members.len(), 2);
    assert!(members.contains(&"i1") && members.contains(&"i2"));
    assert!(b["members"][0]["decision_rev"].is_u64());
    let lone = ok(&e, "attention.batch", json!({"interaction": "i3"})).await;
    assert_eq!(lone["batchable"], false);
    // A question is never batched.
    put_interaction(&e, "q1", "a3", t);
    let q = ok(&e, "attention.batch", json!({"interaction": "q1"})).await;
    assert_eq!(q["batchable"], false);
}

#[tokio::test(flavor = "multi_thread")]
async fn native_deadlines_rank_wake_and_expire_at_the_source() {
    let e = Env::new();
    e.add_run("a1", &e.repo);
    let t = now();
    put_approval(&e, "i1", "a1", "cargo test", t - 1000);
    // No deadline: an ordinary blocking decision.
    let v = ok(&e, "attention.list", json!({})).await;
    assert_eq!(item(&v, "interaction", "i1").unwrap()["class"], 3);
    // A native deadline 30 s out: class 2 under the default 60 s window, with its source.
    attention_ext::record_native_deadline(&e.server, "i1", now() + 30_000);
    let v = ok(&e, "attention.list", json!({})).await;
    let it = item(&v, "interaction", "i1").unwrap();
    assert_eq!(it["class"], 2, "{it}");
    assert_eq!(it["deadline_source"], "native");
    assert!(it["deadline_in_ms"].as_i64().unwrap() <= 30_000);
    assert!(it["explanation"].as_str().unwrap().contains("deadline in"));
    // A gate deadline counts only while the gate is held.
    put_approval(&e, "i2", "a1", "cargo build", t);
    attention_ext::on_gate_opened(&e.server, "i2", &json!({}), Some(Duration::from_secs(20)));
    let v = ok(&e, "attention.list", json!({})).await;
    assert!(item(&v, "interaction", "i2").unwrap()["deadline_ms"].is_null());
    // A passed native deadline: reconciled (expired), no longer listed or answerable.
    attention_ext::record_native_deadline(&e.server, "i1", now() - 1);
    let expired = attention_ext::reconcile_deadlines(&e.server, now());
    assert_eq!(expired, vec!["i1".to_string()]);
    let st = e
        .server
        .with_core(|c| c.interaction("i1").map(|i| i.status));
    assert!(
        st.is_none() || st == Some(InteractionStatus::Expired),
        "{st:?}"
    );
    let v = ok(&e, "attention.list", json!({})).await;
    assert!(item(&v, "interaction", "i1").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn payload_deadlines_from_the_hook_are_recorded() {
    let e = Env::new();
    e.add_run("a1", &e.repo);
    put_approval(&e, "i1", "a1", "cargo test", now());
    attention_ext::on_gate_opened(&e.server, "i1", &json!({"timeout_ms": 45_000}), None);
    let recs = e.server.with_core(|c| attention_ext::deadlines(c));
    assert_eq!(recs["i1"].source, "native");
    assert!(recs["i1"].deadline_ms > now() + 40_000);
}

#[tokio::test(flavor = "multi_thread")]
async fn also_working_lists_busy_runs_without_open_questions() {
    let e = Env::new();
    e.add_run("w1", &e.repo);
    e.add_run("w2", &e.repo);
    e.set_exec("w1", Execution::Working);
    e.set_exec("w2", Execution::Working);
    put_interaction(&e, "q", "w2", now());
    let v = ok(&e, "attention.list", json!({})).await;
    let runs: Vec<&str> = v["also_working"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["run"].as_str().unwrap())
        .collect();
    assert_eq!(runs, vec!["w1"], "{v}");
    // Never ranked as an item.
    assert!(
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["run"] != "w1" || i["key"]["kind"] != "finished_turn")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn inbox_notifications_fire_once_for_newly_urgent_items() {
    let e = Env::new();
    e.add_run("a1", &e.repo);
    put_approval(&e, "i1", "a1", "cargo test", now() - 1000);
    // The first pass only records what is already there (no burst at startup).
    assert!(attention_ext::tick(&e.server).is_empty());
    // Nothing new: nothing notified (the open question was notified when it opened).
    assert!(attention_ext::tick(&e.server).is_empty());
    // Its deadline approaches: one notification, through the ordinary pipeline.
    attention_ext::record_native_deadline(&e.server, "i1", now() + 20_000);
    let n = attention_ext::tick(&e.server);
    assert_eq!(n.len(), 1, "{n:?}");
    assert_eq!(n[0].kind, "attention.deadline");
    assert!(n[0].title.starts_with("Answer soon"));
    assert!(!n[0].channels.is_empty(), "pipeline decided the channels");
    assert!(attention_ext::tick(&e.server).is_empty(), "once");
    // notifications.on.deadline = false filters it (respecting the user's settings).
    let on = vk_config::NotifyOn {
        deadline: false,
        ..Default::default()
    };
    assert!(!crate::notify::kind_enabled(&on, "attention.deadline", ""));
    assert!(crate::notify::kind_enabled(&on, "attention.review", ""));
}

#[tokio::test(flavor = "multi_thread")]
async fn snoozed_items_stay_quiet() {
    let e = Env::new();
    e.add_run("a1", &e.repo);
    put_approval(&e, "i1", "a1", "cargo test", now() - 1000);
    attention_ext::tick(&e.server);
    ok(
        &e,
        "attention.update",
        json!({"key": {"kind": "interaction", "id": "i1"}, "snooze_until_ms": now() + 3_600_000}),
    )
    .await;
    assert!(attention_ext::tick(&e.server).is_empty());
}

#[test]
fn deadline_window_comes_from_the_configuration() {
    attention_ext::set_test_cfg(Some(attention_ext::AttnCfg {
        deadline_window_ms: 5_000,
        also_working: false,
    }));
    assert_eq!(attention_ext::prefs().deadline_window_ms, 5_000);
    attention_ext::set_test_cfg(None);
    let d = vk_config::Config::default();
    assert_eq!(d.ui.interactions.deadline_window.0, Duration::from_secs(60));
    assert!(d.ui.inbox.also_working);
}

// ---- Link run (§3) ------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn link_status_explains_and_offers_verified_runs() {
    let e = Env::new();
    let mut guess = e.add_run("g1", &e.repo);
    guess.integration = "process".into();
    guess.harness_session_id = None;
    guess.pane = "pane-shared".into();
    e.put_run(guess);
    let mut good = e.add_run("v1", &e.repo);
    good.pane = "pane-shared".into();
    e.put_run(good);
    let v = ok(&e, "task.link.status", json!({"run": "g1"})).await;
    assert_eq!(v["verified"], false);
    assert!(!v["reasons"].as_array().unwrap().is_empty());
    assert!(
        v["remedies"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["action"] == "install_integration")
    );
    assert_eq!(v["candidates"][0]["run"], "v1");
    assert_eq!(v["candidates"][0]["same_pane"], true);
    // Tracking the guessed run is still refused; the verified one works.
    assert_eq!(
        reason(call(&e, "task.track", json!({"run": "g1", "criteria": ["x"]})).await),
        "binding_unverified"
    );
    let ok_v = ok(&e, "task.link.status", json!({"run": "v1"})).await;
    assert_eq!(ok_v["verified"], true);
    assert!(ok_v["reasons"].as_array().unwrap().is_empty());
}

// ---- disposable reviewer checkout (§6.1) --------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn reviewer_works_in_a_disposable_checkout_removed_when_done() {
    let e = Env::new();
    let task = tracked(&e, json!(["Looks right"])).await;
    e.write("status.txt", "pass\n");
    e.commit("fix");
    let rq = ok(
        &e,
        "task.review.request_reviewer",
        json!({"task": task, "harness": "claude"}),
    )
    .await;
    let id = rq["request"]["id"].as_str().unwrap().to_string();
    let digest = rq["prompt_digest"].as_str().unwrap().to_string();
    let repo = e.repo.clone();
    t4::set_test_launcher(
        &task,
        Arc::new(move |srv, _rq| {
            let t = now();
            let mut c = srv.core.lock().unwrap();
            let mut tx = Tx::new();
            let mut r = c.run("r1").cloned().unwrap();
            r.id = "rev1".into();
            r.handle = "h-rev1".into();
            r.pane = "pane-rev1".into();
            r.harness_session_id = Some("sess-rev1".into());
            r.turns_completed = 0;
            r.started_at_ms = t;
            r.cwd = Some(repo.to_string_lossy().into_owned());
            tx.run(r);
            srv.commit(&mut c, tx).unwrap();
            Ok(("rev1".into(), "pane-rev1".into()))
        }),
    );
    let started = ok(
        &e,
        "task.review.start_reviewer",
        json!({"request": id, "prompt_digest": digest}),
    )
    .await;
    let co = &started["request"]["checkout"];
    let path = PathBuf::from(co["path"].as_str().expect("a disposable checkout"));
    assert!(path.starts_with(e.server.paths.state.join("reviewers")));
    assert_eq!(
        std::fs::read_to_string(path.join("status.txt")).unwrap(),
        "pass\n"
    );
    // The reviewer editing its checkout never reaches the user's.
    std::fs::write(path.join("status.txt"), "reviewer edit\n").unwrap();
    assert_eq!(
        std::fs::read_to_string(e.repo.join("status.txt")).unwrap(),
        "pass\n"
    );
    // Still running: kept. Ended: removed by the sweep.
    assert!(scratch::sweep(&e.server).is_empty());
    assert!(path.exists());
    let mut r = e.run("rev1");
    r.ended_at_ms = Some(now());
    e.put_run(r);
    assert_eq!(scratch::sweep(&e.server), vec![id.clone()]);
    assert!(!path.exists());
    assert_eq!(events(&e, "review.reviewer_checkout_removed").len(), 1);
    // An explicit disposable checkout together with a named pane is refused.
    if let Ok(rq2) = call(
        &e,
        "task.review.request_reviewer",
        json!({"task": task, "harness": "claude"}),
    )
    .await
    {
        let r2 = call(
            &e,
            "task.review.start_reviewer",
            json!({"request": rq2["request"]["id"], "prompt_digest": rq2["prompt_digest"], "pane": "pane-r1", "checkout": "disposable"}),
        )
        .await;
        assert!(r2.is_err());
    }
}
