//! Lane 3F in-process tests (15 §6.3, §6.4): execution-interval binding of observed commands and
//! pull-request evidence.
//!
//! No real watcher, `gh`, network or harness is involved: the write journal is fed by hand
//! ([`interval::install_manual`] / [`interval::feed`]) and PR lookups are answered from
//! [`pr::test_lookups`]. Commands are reported through `tracking::observe` exactly like the hook
//! shim does; their intervals are recorded through the same `begin`/`end` functions the worker
//! thread runs.

use super::*;
use vk_review::pr_evidence::{self as pe, ChecksRollup, Lookup, PrFacts, PrState};

// ---- helpers --------------------------------------------------------------------------------

fn icfg() -> interval::IntervalConfig {
    interval::IntervalConfig {
        settle_ms: 0,
        ..Default::default()
    }
}

/// A repository whose run executed `commands` (all exit 0) after a committed fix. The tree is
/// clean afterwards, `target/` is ignored and a manual, armed journal covers the checkout.
async fn command_env(
    commands: &[&str],
) -> (
    Env,
    String,
    std::sync::Arc<Mutex<vk_review::interval::WriteJournal>>,
) {
    let e = Env::new();
    e.write(".gitignore", "target/\n");
    e.commit("ignore build output");
    e.add_run("r1", &e.repo);
    e.turn("r1", "Make the tests pass", &[], "starting");
    let task = track(
        &e,
        "r1",
        json!([{"text": "Tests pass", "checks": ["unit"]}]),
    )
    .await;
    e.write("status.txt", "pass\n");
    e.commit("fix");
    let tools: Vec<(&str, i32)> = commands.iter().map(|c| (*c, 0)).collect();
    e.turn("r1", "go on", &tools, "done");
    let journal = interval::install_manual(&e.repo, 0, 1000);
    (e, task, journal)
}

/// `(run, item id)` of the n-th observed command of the package.
fn cmd(pkg: &Value, n: usize) -> (String, String) {
    let c = &pkg["observed_commands"][n];
    (
        c["run_id"].as_str().unwrap().to_string(),
        c["id"].as_str().unwrap().to_string(),
    )
}

fn start(e: &Env, pkg: &Value, n: usize, start_ms: i64) {
    let (run, item) = cmd(pkg, n);
    let command = pkg["observed_commands"][n]["command"].as_str().unwrap();
    interval::begin(
        &e.server,
        &icfg(),
        &run,
        &item,
        Some(command),
        &e.repo,
        start_ms,
    )
    .expect("the run's directory is a git checkout");
}

fn finish(e: &Env, pkg: &Value, n: usize, end_ms: i64) -> vk_review::interval::IntervalBinding {
    let (run, item) = cmd(pkg, n);
    interval::end(&e.server, &icfg(), &run, &item, end_ms, false).expect("an open interval")
}

fn reasons_of(c: &Value) -> Vec<String> {
    c["reasons"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r.as_str().unwrap().to_string())
        .collect()
}

// ---- execution-interval binding -------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_quiet_covered_command_is_bound_to_the_subject_and_supports_its_check() {
    let (e, task, _j) = command_env(&["sh ci/test.sh"]).await;
    // Before any interval record: the command ran, but nothing established its code subject.
    let pkg = review(&e, &task).await;
    let oc = &pkg["observed_commands"][0];
    assert_eq!(oc["subject"], "unbound");
    assert_eq!(oc["label"], "Command passed · code binding unverified");
    assert_eq!(oc["interval"]["status"], "not_collected");
    assert_eq!(criterion(&pkg, "Tests pass")["status"], "unknown");

    start(&e, &pkg, 0, 1000);
    let b = finish(&e, &pkg, 0, 2000);
    assert!(b.is_bound(), "{:?}", b.reasons);

    let pkg = review(&e, &task).await;
    let oc = &pkg["observed_commands"][0];
    assert_eq!(oc["subject"], pkg["subject"]["id"]);
    assert_eq!(oc["label"], "Command passed");
    assert_eq!(oc["interval"]["status"], "bound");
    assert!(
        oc["interval"]["explanation"][0]
            .as_str()
            .unwrap()
            .contains("stable")
    );
    // The command's tools were probed when it ended, so the evidence carries an environment
    // identity and is fresh, not stale.
    let c = criterion(&pkg, "Tests pass");
    assert_eq!(c["status"], "supported", "{c}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_write_and_revert_during_the_command_is_unbound_even_with_matching_states() {
    let (e, task, j) = command_env(&["sh ci/test.sh"]).await;
    let pkg = review(&e, &task).await;
    start(&e, &pkg, 0, 1000);
    // The agent edited a tracked file and reverted it before the command ended: the checkout
    // state at the start and at the end are identical.
    interval::feed(&e.repo, "status.txt", 1500);
    let b = finish(&e, &pkg, 0, 2000);
    assert!(!b.is_bound());
    assert!(matches!(
        b.reasons[0],
        vk_review::interval::UnboundReason::WritesObserved { .. }
    ));
    assert_eq!(j.lock().unwrap().len(), 1);

    let pkg = review(&e, &task).await;
    let oc = &pkg["observed_commands"][0];
    assert_eq!(oc["subject"], "unbound");
    assert_eq!(oc["interval"]["status"], "unbound");
    assert_eq!(oc["interval"]["reasons"][0]["kind"], "writes_observed");
    let why = oc["interval"]["explanation"].as_array().unwrap();
    assert!(
        why.last()
            .unwrap()
            .as_str()
            .unwrap()
            .contains("stable commit")
    );
    assert_eq!(criterion(&pkg, "Tests pass")["status"], "unknown");
}

#[tokio::test(flavor = "multi_thread")]
async fn ignored_writes_do_not_unbind_but_other_writes_do() {
    let (e, task, _j) = command_env(&["sh ci/test.sh", "sh ci/test.sh"]).await;
    let pkg = review(&e, &task).await;
    // Command 0 wrote build output only; command 1 wrote a source file.
    start(&e, &pkg, 0, 1000);
    interval::feed(&e.repo, "target/debug/app.o", 1200);
    interval::feed(&e.repo, ".git/index.lock", 1300);
    assert!(finish(&e, &pkg, 0, 2000).is_bound());
    start(&e, &pkg, 1, 3000);
    interval::feed(&e.repo, "src/new.rs", 3500);
    let b = finish(&e, &pkg, 1, 4000);
    assert!(!b.is_bound());
    let pkg = review(&e, &task).await;
    assert_eq!(pkg["observed_commands"][0]["interval"]["status"], "bound");
    assert_eq!(pkg["observed_commands"][1]["interval"]["status"], "unbound");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_watcher_that_started_late_or_lost_events_never_binds() {
    // Armed after the command started.
    let (e, task, _j) = command_env(&["sh ci/test.sh"]).await;
    let pkg = review(&e, &task).await;
    interval::install_manual(&e.repo, 1500, 1000);
    start(&e, &pkg, 0, 1000);
    let b = finish(&e, &pkg, 0, 2000);
    assert_eq!(
        b.reasons,
        vec![vk_review::interval::UnboundReason::WatcherLate]
    );

    // An overflow reported after the command still hides events from inside it.
    let (e, task, j) = command_env(&["sh ci/test.sh"]).await;
    let pkg = review(&e, &task).await;
    start(&e, &pkg, 0, 1000);
    j.lock().unwrap().heartbeat(900);
    j.lock().unwrap().mark_rescan(2600);
    let b = finish(&e, &pkg, 0, 2000);
    assert_eq!(
        b.reasons,
        vec![vk_review::interval::UnboundReason::WatcherGap]
    );
    let pkg = review(&e, &task).await;
    assert_eq!(pkg["observed_commands"][0]["subject"], "unbound");

    // No watcher for the checkout at all (the `none` backend).
    let (e, task, _j) = command_env(&["sh ci/test.sh"]).await;
    interval::remove_watcher(&e.repo);
    let cfg = interval::IntervalConfig {
        watcher: "none".into(),
        settle_ms: 0,
        ..Default::default()
    };
    let pkg = review(&e, &task).await;
    let (run, item) = cmd(&pkg, 0);
    let rec = interval::begin(
        &e.server,
        &cfg,
        &run,
        &item,
        Some("sh ci/test.sh"),
        &e.repo,
        1000,
    )
    .unwrap();
    assert_eq!(rec.watcher, "none");
    let b = interval::end(&e.server, &cfg, &run, &item, 2000, false).unwrap();
    assert_eq!(
        b.reasons,
        vec![vk_review::interval::UnboundReason::WatcherNotArmed]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_checkout_that_changed_between_start_and_end_is_unbound() {
    let (e, task, _j) = command_env(&["sh ci/test.sh"]).await;
    let pkg = review(&e, &task).await;
    start(&e, &pkg, 0, 1000);
    // The command left an untracked file behind (nothing fed to the journal).
    e.write("scratch.txt", "left over\n");
    let b = finish(&e, &pkg, 0, 2000);
    assert!(
        b.reasons
            .contains(&vk_review::interval::UnboundReason::SubjectChanged)
    );
    let pkg = review(&e, &task).await;
    assert_eq!(pkg["observed_commands"][0]["subject"], "unbound");
}

#[tokio::test(flavor = "multi_thread")]
async fn another_writer_in_the_checkout_unbinds() {
    let (e, task, _j) = command_env(&["sh ci/test.sh"]).await;
    e.add_run("r2", &e.repo);
    e.set_exec("r2", Execution::Working);
    let pkg = review(&e, &task).await;
    start(&e, &pkg, 0, 1000);
    let b = finish(&e, &pkg, 0, 2000);
    assert!(matches!(
        b.reasons.last(),
        Some(vk_review::interval::UnboundReason::OtherWriter { .. })
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn interval_records_are_listed_and_the_watcher_status_is_reported() {
    let (e, task, _j) = command_env(&["sh ci/test.sh"]).await;
    let pkg = review(&e, &task).await;
    start(&e, &pkg, 0, 1000);
    let l = ok(&e, "task.review.intervals", json!({"task": task})).await;
    assert_eq!(l["intervals"][0]["decided"], false);
    assert_eq!(l["intervals"][0]["watcher"], "manual");
    finish(&e, &pkg, 0, 2000);
    let l = ok(&e, "task.review.intervals", json!({"task": task})).await;
    assert_eq!(l["intervals"][0]["decided"], true);
    assert_eq!(l["intervals"][0]["binding"]["status"], "bound");
    assert_eq!(l["intervals"][0]["command"], "sh ci/test.sh");
    let st = ok(&e, "task.review.interval_status", json!({})).await;
    assert_eq!(st["enabled"], true);
    assert!(
        st["checkouts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["armed"] == true && c["watcher"] == "manual")
    );
    assert_eq!(st["harnesses"][0], "claude");
}

// ---- pull-request evidence ------------------------------------------------------------------

fn facts(head: &str, state: PrState, draft: bool, checks: ChecksRollup) -> PrFacts {
    PrFacts {
        identity: pe::parse_pr_url("https://github.com/acme/app/pull/7").unwrap(),
        target_branch: "main".into(),
        head_branch: Some("feat".into()),
        head_sha: head.into(),
        state,
        draft,
        review_decision: None,
        checks,
    }
}

fn set_lookup(e: &Env, l: Lookup) {
    let key = std::fs::canonicalize(&e.repo).unwrap();
    pr::test_lookups().lock().unwrap().insert(key, l);
}

async fn pr_env() -> (Env, String, String) {
    let e = Env::new();
    git(
        &e.repo,
        &["remote", "add", "origin", "git@github.com:acme/app.git"],
    );
    e.add_run("r1", &e.repo);
    e.turn("r1", "Open a pull request", &[], "ok");
    let task = track(
        &e,
        "r1",
        json!([{"text": "PR is open", "evaluation": "external"}]),
    )
    .await;
    e.write("status.txt", "pass\n");
    let head = e.commit("fix");
    (e, task, head)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pr_observation_binds_to_its_exact_head_and_a_moved_head_invalidates_it() {
    let (e, task, h1) = pr_env().await;
    let pkg = review(&e, &task).await;
    assert_eq!(criterion(&pkg, "PR is open")["status"], "needs_judgment");
    assert_eq!(pkg["pr"]["observations"].as_array().unwrap().len(), 0);
    assert_eq!(pkg["pr"]["observe"]["available"], true);

    set_lookup(
        &e,
        Lookup::Observed {
            facts: facts(&h1, PrState::Open, false, ChecksRollup::Passing),
        },
    );
    let r = ok(
        &e,
        "task.pr.observe",
        json!({"task": task, "idempotency_key": "o1"}),
    )
    .await;
    assert_eq!(r["observation"]["lookup"]["kind"], "observed");
    assert_eq!(r["observation"]["lookup"]["head_sha"], h1.as_str());
    assert_eq!(r["observation"]["lookup"]["target_branch"], "main");
    assert_eq!(r["observation"]["authorization_scope"], "gh_cli");
    // Idempotent per key.
    let again = ok(
        &e,
        "task.pr.observe",
        json!({"task": task, "idempotency_key": "o1"}),
    )
    .await;
    assert_eq!(again["replayed"], true);
    let list = ok(&e, "task.pr.list", json!({"task": task})).await;
    assert_eq!(list["observations"].as_array().unwrap().len(), 1);
    assert_eq!(events(&e, "review.pr_observed").len(), 1);

    let pkg = review(&e, &task).await;
    let a = &pkg["pr"]["observations"][0]["assessment"];
    assert_eq!(a["binding"], "bound", "{a}");
    assert_eq!(a["outcome"], "passed");
    assert_eq!(criterion(&pkg, "PR is open")["status"], "supported");

    // The branch moves on: the old observation no longer says anything about the current
    // revision, but still belongs to the old one.
    e.write("status.txt", "pass again\n");
    let h2 = e.commit("more");
    let pkg = review(&e, &task).await;
    assert_eq!(pkg["subject"]["head_sha"], h2.as_str());
    let a = &pkg["pr"]["observations"][0]["assessment"];
    assert_eq!(a["binding"], "unbound");
    assert_eq!(a["unbound_reason"]["reason"], "head_changed");
    assert_eq!(criterion(&pkg, "PR is open")["status"], "needs_judgment");

    // A new observation of the new head is a new row and a head-changed event.
    set_lookup(
        &e,
        Lookup::Observed {
            facts: facts(&h2, PrState::Open, false, ChecksRollup::None),
        },
    );
    ok(&e, "task.pr.observe", json!({"task": task})).await;
    assert_eq!(events(&e, "review.pr_head_changed").len(), 1);
    let pkg = review(&e, &task).await;
    let rows = pkg["pr"]["observations"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["current"], false);
    assert_eq!(rows[1]["current"], true);
    assert_eq!(criterion(&pkg, "PR is open")["status"], "supported");
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_lookups_failing_checks_and_merged_prs_never_pass() {
    let (e, task, h1) = pr_env().await;
    // Offline: recorded as such, outcome unknown.
    set_lookup(
        &e,
        Lookup::Failed {
            reason: "gh is not authenticated".into(),
        },
    );
    let r = ok(&e, "task.pr.observe", json!({"task": task})).await;
    assert_eq!(r["observation"]["lookup"]["kind"], "failed");
    let pkg = review(&e, &task).await;
    assert_eq!(criterion(&pkg, "PR is open")["status"], "unknown");
    assert_eq!(
        pkg["pr"]["observations"][0]["assessment"]["outcome"],
        "unknown"
    );

    // Failing checks: failed for this revision.
    set_lookup(
        &e,
        Lookup::Observed {
            facts: facts(&h1, PrState::Open, false, ChecksRollup::Failing),
        },
    );
    ok(&e, "task.pr.observe", json!({"task": task})).await;
    let pkg = review(&e, &task).await;
    assert_eq!(criterion(&pkg, "PR is open")["status"], "failed");

    // Merged: shown, never turned into a pass (merge observation is an extension point).
    set_lookup(
        &e,
        Lookup::Observed {
            facts: facts(&h1, PrState::Merged, false, ChecksRollup::Passing),
        },
    );
    ok(&e, "task.pr.observe", json!({"task": task})).await;
    let pkg = review(&e, &task).await;
    let last = pkg["pr"]["observations"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(last["assessment"]["unbound_reason"]["reason"], "merged");
    assert_ne!(criterion(&pkg, "PR is open")["status"], "supported");

    // No pull request for the branch: unknown, not a failure.
    set_lookup(&e, Lookup::NoPr);
    ok(&e, "task.pr.observe", json!({"task": task})).await;
    let pkg = review(&e, &task).await;
    assert_eq!(criterion(&pkg, "PR is open")["status"], "unknown");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pasted_url_is_a_claim_and_never_confirmation() {
    let (e, task, _h) = pr_env().await;
    let r = ok(
        &e,
        "task.pr.claim",
        json!({"task": task, "url": "https://github.com/acme/app/pull/7", "text": "PR is open"}),
    )
    .await;
    assert_eq!(r["claim"]["source"], "pasted_url");
    assert!(r["label"].as_str().unwrap().contains("not confirmed"));
    assert_eq!(events(&e, "review.pr_claimed").len(), 1);
    let pkg = review(&e, &task).await;
    assert_eq!(pkg["pr"]["claims"][0]["confirmed"], false);
    let c = criterion(&pkg, "PR is open");
    assert_eq!(c["status"], "needs_judgment", "{c}");
    assert!(
        reasons_of(c).iter().any(|r| r.contains("not evidence")),
        "{c}"
    );
    // Garbage is refused; a non-URL string is recorded as an unparsed claim, still unconfirmed.
    assert!(
        call(&e, "task.pr.claim", json!({"task": task, "url": ""}))
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn observation_rejects_option_like_references_and_unknown_criteria() {
    let (e, task, _h) = pr_env().await;
    for bad in [
        json!({"pr": "--exec=evil"}),
        json!({"pr": "https://evil.example/x"}),
    ] {
        let mut p = json!({"task": task});
        p.as_object_mut()
            .unwrap()
            .extend(bad.as_object().unwrap().clone());
        assert!(call(&e, "task.pr.observe", p).await.is_err());
    }
    assert!(
        call(
            &e,
            "task.pr.observe",
            json!({"task": task, "criteria": ["no-such-criterion"]})
        )
        .await
        .is_err()
    );
    assert!(pr::observations_of(&e.server.core.lock().unwrap(), &task).is_empty());
}

#[test]
fn observing_is_full_scope_only() {
    use crate::api::{PaneScope, pane_scope_of};
    assert_eq!(pane_scope_of("task.pr.observe"), PaneScope::Forbidden);
    assert_ne!(pane_scope_of("task.pr.claim"), PaneScope::Forbidden);
    assert_ne!(pane_scope_of("task.pr.list"), PaneScope::Forbidden);
    assert_ne!(pane_scope_of("task.review.intervals"), PaneScope::Forbidden);
}
