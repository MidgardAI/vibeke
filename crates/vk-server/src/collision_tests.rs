//! The collision tracker and claims (05 §10; 3A): adapter signals, watcher and `git status`
//! attribution, the rules end to end (records, events, notifications), the actions, claims and
//! their enforcement, the sweep and the API surface. Fakes only: no harness runs, the platform
//! watcher is replaced by `watch::feed_fs`, the open-file probe is injected, and git runs only
//! in temp repositories.

use super::*;
use crate::api::{PaneScope, dispatch, pane_scope_of};
use crate::hardening::testkit::{pane_ctx, sample_pane, sample_run, server, user};
use std::process::{Command, Stdio};
use std::time::Duration;
use vk_proto::model::Execution;

struct Fx {
    _dir: tempfile::TempDir,
    s: Arc<Server>,
    root: String,
}

fn test_cfg() -> vk_config::Collision {
    vk_config::Collision {
        watcher: "none".into(),
        settle: Duration::ZERO,
        ..Default::default()
    }
}

/// A server and a checkout (`<tmp>/repo` with a `.git` marker) with no runs yet.
fn fx(name: &str) -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let s = server(dir.path(), name);
    let root = dir.path().canonicalize().unwrap().join("repo");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(root.join("src/auth")).unwrap();
    set_config(&s, test_cfg());
    Fx {
        _dir: dir,
        s,
        root: root.to_string_lossy().into_owned(),
    }
}

fn add_run(f: &Fx, id: &str, pane: &str, harness: &str, exec: Execution) -> AgentRun {
    add_run_in(f, id, pane, harness, exec, &f.root)
}

fn add_run_in(f: &Fx, id: &str, pane: &str, harness: &str, exec: Execution, cwd: &str) -> AgentRun {
    let mut run = sample_run(id, pane);
    run.harness = harness.into();
    run.cwd = Some(cwd.into());
    run.execution.value = exec;
    let mut c = f.s.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.pane(sample_pane(pane, "w1"));
    tx.run(run.clone());
    f.s.commit(&mut c, tx).unwrap();
    run
}

fn set_exec(s: &Server, id: &str, e: Execution) {
    let mut c = s.core.lock().unwrap();
    let mut r = c.run(id).cloned().unwrap();
    r.execution.value = e;
    let mut tx = Tx::new();
    tx.run(r);
    s.commit(&mut c, tx).unwrap();
}

fn end_run(s: &Server, id: &str) {
    let mut c = s.core.lock().unwrap();
    let mut r = c.run(id).cloned().unwrap();
    r.execution.value = Execution::Exited;
    r.ended_at_ms = Some(now_ms());
    let mut tx = Tx::new();
    tx.run(r);
    s.commit(&mut c, tx).unwrap();
}

fn abs(f: &Fx, rel: &str) -> String {
    format!("{}/{rel}", f.root)
}

fn post(f: &Fx, run: &AgentRun, tool: &str, rel: &str) {
    let p = json!({"tool_name": tool, "tool_use_id": format!("t-{rel}"), "tool_input": {"file_path": abs(f, rel)}});
    observe(&f.s, run, "PostToolUse", &p);
}

fn pre(f: &Fx, run: &AgentRun, tool: &str, rel: &str) {
    let p = json!({"tool_name": tool, "tool_use_id": format!("t-{rel}"), "tool_input": {"file_path": abs(f, rel)}});
    observe(&f.s, run, "PreToolUse", &p);
}

fn open(s: &Server) -> Vec<vc::CollisionRec> {
    s.collision.inner.lock().unwrap().open.clone()
}

fn events(s: &Server, kind: &str) -> Vec<vk_store::Event> {
    let evs = s.with_core(|c| {
        c.store
            .events_after(0, 100_000, &[kind.to_string()])
            .unwrap()
    });
    for e in &evs {
        let v = serde_json::to_value(e).unwrap();
        let problems = crate::api_schema::validate_event(&v);
        assert!(problems.is_empty(), "event {v}: {problems:?}");
    }
    evs
}

fn notifications(s: &Server) -> Vec<vk_proto::model::Notification> {
    s.with_core(|c| {
        c.notifications
            .iter()
            .filter(|n| n.kind == "collision")
            .cloned()
            .collect()
    })
}

fn touches(s: &Server, root: &str) -> Vec<vc::Touch> {
    s.collision
        .inner
        .lock()
        .unwrap()
        .roots
        .get(root)
        .map(|r| r.tracker.touches().cloned().collect())
        .unwrap_or_default()
}

async fn ok(s: &Arc<Server>, ctx: &Ctx, method: &str, p: Value) -> Value {
    let problems = crate::api_schema::validate_params(method, &p);
    assert!(problems.is_empty(), "{method} params {p}: {problems:?}");
    let v = dispatch(s, ctx, method, &p)
        .await
        .unwrap_or_else(|e| panic!("{method} {p}: {e:?}"));
    let problems = crate::api_schema::validate_result(method, &v);
    assert!(problems.is_empty(), "{method} result {v}: {problems:?}");
    v
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let st = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(st.success(), "git {args:?}");
}

/// A real repository in `<tmp>/real` with one commit, for the `git status` poll and the HEAD
/// of "start a fresh task".
fn real_repo(f: &Fx) -> String {
    let root = std::path::Path::new(&f.root).parent().unwrap().join("real");
    std::fs::create_dir_all(root.join("src")).unwrap();
    git(&root, &["init", "-q"]);
    std::fs::write(root.join("a.txt"), "one\n").unwrap();
    std::fs::write(root.join("src/b.txt"), "two\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "init"]);
    std::fs::canonicalize(&root)
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

// ---- rules end to end -----------------------------------------------------------------------

#[test]
fn two_runs_on_one_file_raise_a_high_collision_with_event_and_one_notification() {
    let f = fx("high");
    let a = add_run(&f, "ra", "pa", "claude", Execution::Working);
    let b = add_run(&f, "rb", "pb", "codex", Execution::Working);
    post(&f, &a, "Edit", "src/auth.ts");
    assert!(open(&f.s).is_empty(), "one writer is not a collision");
    post(&f, &b, "Write", "src/auth.ts");
    let recs = open(&f.s);
    assert_eq!(recs.len(), 1);
    let r = &recs[0];
    assert_eq!(r.severity, vc::Severity::High);
    assert_eq!(r.runs, vec!["ra".to_string(), "rb".to_string()]);
    assert_eq!(r.paths.len(), 1);
    assert_eq!(r.paths[0].path, "src/auth.ts");
    assert_eq!(r.timeline.len(), 2, "both writes are on the timeline");
    assert_eq!(r.root, f.root);

    let ev = events(&f.s, "task.collision_detected");
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].data["severity"], "high");
    assert_eq!(ev[0].data["reason"], "same_file");
    assert_eq!(ev[0].data["created"], true);
    assert_eq!(ev[0].data["paths"], json!(["src/auth.ts"]));

    let n = notifications(&f.s);
    assert_eq!(n.len(), 1);
    assert!(
        n[0].title.contains("2 agents editing src/auth.ts"),
        "{}",
        n[0].title
    );
    // The same path set again inside the window: no event, no second notification.
    post(&f, &b, "Edit", "src/auth.ts");
    assert_eq!(events(&f.s, "task.collision_detected").len(), 1);
    assert_eq!(notifications(&f.s).len(), 1);
    // A second file joins the record and announces again.
    post(&f, &a, "Edit", "src/auth/login.ts");
    post(&f, &b, "Edit", "src/auth/login.ts");
    assert_eq!(open(&f.s).len(), 1, "same run pair, same record");
    assert_eq!(open(&f.s)[0].paths.len(), 2);
    assert_eq!(events(&f.s, "task.collision_detected").len(), 2);
    assert_eq!(notifications(&f.s).len(), 2);
}

#[test]
fn same_directory_is_a_low_hint_without_a_notification() {
    let f = fx("low");
    let a = add_run(&f, "ra", "pa", "claude", Execution::Working);
    let b = add_run(&f, "rb", "pb", "claude", Execution::Working);
    post(&f, &a, "Edit", "src/auth/login.ts");
    post(&f, &b, "Edit", "src/auth/logout.ts");
    let recs = open(&f.s);
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].severity, vc::Severity::Low);
    assert_eq!(
        events(&f.s, "task.collision_detected")[0].data["reason"],
        "same_dir"
    );
    assert!(notifications(&f.s).is_empty(), "low is a sidebar hint only");
}

#[test]
fn editing_what_another_run_read_is_medium_and_notifies() {
    let f = fx("medium");
    let a = add_run(&f, "ra", "pa", "claude", Execution::Working);
    let b = add_run(&f, "rb", "pb", "codex", Execution::Working);
    post(&f, &a, "Read", "api/auth.ts");
    assert!(open(&f.s).is_empty());
    post(&f, &b, "Edit", "api/auth.ts");
    let recs = open(&f.s);
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].severity, vc::Severity::Medium);
    assert_eq!(
        recs[0].paths[0].reason,
        vc::Reason::ReadThenEdited {
            editor: Some("rb".into()),
            reader: "ra".into()
        }
    );
    assert_eq!(notifications(&f.s).len(), 1);
}

#[test]
fn isolated_checkouts_never_collide() {
    let f = fx("isolated");
    let a = add_run(&f, "ra", "pa", "claude", Execution::Working);
    // A task worktree has its own `.git` file and so its own root.
    let wt = std::path::Path::new(&f.root).parent().unwrap().join("wt");
    std::fs::create_dir_all(&wt).unwrap();
    std::fs::write(wt.join(".git"), "gitdir: x").unwrap();
    let wt = std::fs::canonicalize(&wt)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let b = add_run_in(&f, "rb", "pb", "claude", Execution::Working, &wt);
    post(&f, &a, "Edit", "src/auth.ts");
    let p = json!({"tool_name": "Edit", "tool_use_id": "x", "tool_input": {"file_path": format!("{wt}/src/auth.ts")}});
    observe(&f.s, &b, "PostToolUse", &p);
    assert!(open(&f.s).is_empty());
}

#[test]
fn paths_outside_the_checkout_and_ignored_dirs_are_not_tracked() {
    let f = fx("outside");
    let a = add_run(&f, "ra", "pa", "claude", Execution::Working);
    let b = add_run(&f, "rb", "pb", "claude", Execution::Working);
    for run in [&a, &b] {
        let p = json!({"tool_name": "Edit", "tool_input": {"file_path": "/etc/hosts"}});
        observe(&f.s, run, "PostToolUse", &p);
        post(&f, run, "Edit", "node_modules/x/index.js");
        post(&f, run, "Edit", ".git/config");
    }
    assert!(open(&f.s).is_empty());
    assert!(touches(&f.s, &f.root).is_empty());
}

#[test]
fn codex_apply_patch_and_opencode_file_events_feed_the_tracker() {
    let f = fx("patch");
    let a = add_run(&f, "ra", "pa", "codex", Execution::Working);
    let b = add_run(&f, "rb", "pb", "opencode", Execution::Working);
    let patch = format!(
        "*** Begin Patch\n*** Update File: {}\n@@\n-x\n+y\n*** End Patch",
        abs(&f, "src/auth.ts")
    );
    observe(
        &f.s,
        &a,
        "PostToolUse",
        &json!({"tool_name": "apply_patch", "tool_input": {"input": patch}}),
    );
    file_changed(&f.s, &b, &abs(&f, "src/auth.ts"), "modify");
    let recs = open(&f.s);
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].severity, vc::Severity::High);
}

#[test]
fn a_disabled_tracker_records_nothing() {
    let f = fx("off");
    set_config(
        &f.s,
        vk_config::Collision {
            enabled: false,
            ..test_cfg()
        },
    );
    let a = add_run(&f, "ra", "pa", "claude", Execution::Working);
    let b = add_run(&f, "rb", "pb", "claude", Execution::Working);
    post(&f, &a, "Edit", "x.rs");
    post(&f, &b, "Edit", "x.rs");
    assert!(open(&f.s).is_empty());
    assert!(touches(&f.s, &f.root).is_empty());
}

// ---- watcher and git attribution ------------------------------------------------------------

#[test]
fn watcher_events_are_attributed_in_flight_then_working_and_ambiguous() {
    let f = fx("watch");
    let a = add_run(&f, "ra", "pa", "claude", Execution::Working);
    let _b = add_run(&f, "rb", "pb", "claude", Execution::Working);
    let now = now_ms();
    // Nobody reported this path: both runs are working, so the change is ambiguous.
    watch::feed_fs(&f.s, &f.root, "src/x.rs", vc::Op::Modify, now);
    watch::tick(&f.s, now + 1);
    let t = touches(&f.s, &f.root);
    assert_eq!(t.len(), 1);
    assert!(t[0].run.is_none());
    assert_eq!(t[0].candidates, vec!["ra".to_string(), "rb".to_string()]);
    assert_eq!(t[0].source, vc::Source::Watcher);
    assert!(
        open(&f.s).is_empty(),
        "ambiguous touches never collide with each other"
    );

    // A run reported an in-flight edit of this path: the event is its.
    pre(&f, &a, "Edit", "src/y.rs");
    watch::feed_fs(&f.s, &f.root, "src/y.rs", vc::Op::Modify, now + 10);
    watch::tick(&f.s, now + 11);
    let y: Vec<vc::Touch> = touches(&f.s, &f.root)
        .into_iter()
        .filter(|t| t.path == "src/y.rs")
        .collect();
    assert_eq!(y.len(), 1);
    assert_eq!(y[0].run.as_deref(), Some("ra"));
    assert_eq!(y[0].source, vc::Source::Watcher);
}

#[test]
fn a_reported_edit_explains_its_own_watcher_event() {
    let f = fx("explained");
    let a = add_run(&f, "ra", "pa", "claude", Execution::Working);
    let _b = add_run(&f, "rb", "pb", "claude", Execution::Working);
    post(&f, &a, "Edit", "src/z.rs");
    let before = touches(&f.s, &f.root).len();
    let now = now_ms();
    watch::feed_fs(&f.s, &f.root, "src/z.rs", vc::Op::Modify, now);
    watch::tick(&f.s, now + 1);
    assert_eq!(
        touches(&f.s, &f.root).len(),
        before,
        "no ambiguous echo of the report"
    );
    assert!(open(&f.s).is_empty());
}

#[test]
fn a_change_while_nobody_works_is_not_an_agents() {
    let f = fx("idle");
    let _a = add_run(&f, "ra", "pa", "claude", Execution::Idle);
    let _b = add_run(&f, "rb", "pb", "claude", Execution::Idle);
    let now = now_ms();
    watch::feed_fs(&f.s, &f.root, "src/x.rs", vc::Op::Modify, now);
    watch::tick(&f.s, now + 1);
    assert!(touches(&f.s, &f.root).is_empty());
}

#[test]
fn a_lone_run_is_not_watched() {
    let f = fx("lone");
    let _a = add_run(&f, "ra", "pa", "claude", Execution::Working);
    let now = now_ms();
    watch::feed_fs(&f.s, &f.root, "src/x.rs", vc::Op::Modify, now);
    watch::tick(&f.s, now + 1);
    assert!(touches(&f.s, &f.root).is_empty());
    assert!(watch::shared_roots(&f.s).is_empty());
}

#[test]
fn aggressive_attribution_names_the_run_whose_process_wrote_the_file() {
    let f = fx("fd");
    set_config(
        &f.s,
        vk_config::Collision {
            fs_attribution: vk_config::FsAttribution::Aggressive,
            ..test_cfg()
        },
    );
    let _a = add_run(&f, "ra", "pa", "claude", Execution::Working);
    let _b = add_run(&f, "rb", "pb", "claude", Execution::Working);
    // pb's shell is pid 4100; the writer (pid 4242) is a child of it.
    {
        let mut c = f.s.core.lock().unwrap();
        let mut p = sample_pane("pb", "w1");
        p.child_pid = Some(4100);
        let mut tx = Tx::new();
        tx.pane(p);
        f.s.commit(&mut c, tx).unwrap();
    }
    watch::set_probe(
        &f.s,
        watch::FdProbe {
            writers: Arc::new(|_| vec![4242]),
            ancestry: Arc::new(|pid| {
                if pid == 4242 {
                    vec![4242, 4100, 1]
                } else {
                    vec![pid]
                }
            }),
        },
    );
    let now = now_ms();
    watch::feed_fs(&f.s, &f.root, "src/x.rs", vc::Op::Modify, now);
    watch::tick(&f.s, now + 1);
    let t = touches(&f.s, &f.root);
    assert_eq!(t.len(), 1);
    assert_eq!(
        t[0].run.as_deref(),
        Some("rb"),
        "the open-file writer decides, not 'both working'"
    );
}

#[test]
fn off_attribution_stops_the_watcher_path() {
    let f = fx("fsoff");
    set_config(
        &f.s,
        vk_config::Collision {
            fs_attribution: vk_config::FsAttribution::Off,
            ..test_cfg()
        },
    );
    let _a = add_run(&f, "ra", "pa", "claude", Execution::Working);
    let _b = add_run(&f, "rb", "pb", "claude", Execution::Working);
    let now = now_ms();
    watch::feed_fs(&f.s, &f.root, "src/x.rs", vc::Op::Modify, now);
    watch::tick(&f.s, now + 1);
    assert!(touches(&f.s, &f.root).is_empty());
    // Adapter reports still work.
    let a = run_by_id(&f.s, "ra").unwrap();
    post(&f, &a, "Edit", "src/x.rs");
    assert_eq!(touches(&f.s, &f.root).len(), 1);
}

#[test]
fn the_git_poll_attributes_shell_edits_and_catches_a_second_writer() {
    let f = fx("gitpoll");
    let root = real_repo(&f);
    let cfg = test_cfg();
    let a = add_run_in(&f, "ra", "pa", "claude", Execution::Working, &root);
    let b = add_run_in(&f, "rb", "pb", "claude", Execution::Idle, &root);
    let runs = |s: &Server| -> Vec<AgentRun> {
        vec![run_by_id(s, "ra").unwrap(), run_by_id(s, "rb").unwrap()]
    };
    // The first snapshot is the baseline and reports nothing.
    watch::poll_now(&f.s, &cfg, &root, &runs(&f.s), 1000);
    assert!(touches(&f.s, &root).is_empty());
    // A shell edit by the only working run is attributed to it.
    std::fs::write(std::path::Path::new(&root).join("a.txt"), "one\ntwo\n").unwrap();
    watch::poll_now(&f.s, &cfg, &root, &runs(&f.s), 2000);
    let t = touches(&f.s, &root);
    assert_eq!(t.len(), 1);
    assert_eq!(t[0].run.as_deref(), Some("ra"));
    assert_eq!(t[0].source, vc::Source::Git);
    assert_eq!(t[0].path, "a.txt");
    assert!(open(&f.s).is_empty());
    // The other run takes over and rewrites the same file: now two runs wrote it.
    set_exec(&f.s, "ra", Execution::Idle);
    set_exec(&f.s, "rb", Execution::Working);
    std::fs::write(
        std::path::Path::new(&root).join("a.txt"),
        "one\ntwo\nthree\n",
    )
    .unwrap();
    watch::poll_now(&f.s, &cfg, &root, &runs(&f.s), 3000);
    let recs = open(&f.s);
    assert_eq!(recs.len(), 1, "{recs:?}");
    assert_eq!(recs[0].severity, vc::Severity::High);
    assert_eq!(recs[0].runs, vec!["ra".to_string(), "rb".to_string()]);
    // An unchanged tree reports nothing more.
    let n = touches(&f.s, &root).len();
    watch::poll_now(&f.s, &cfg, &root, &runs(&f.s), 4000);
    assert_eq!(touches(&f.s, &root).len(), n);
    let _ = (a, b);
}

#[test]
fn the_poll_only_runs_while_an_agent_is_working_and_on_its_period() {
    let f = fx("pollgate");
    let root = real_repo(&f);
    let _a = add_run_in(&f, "ra", "pa", "claude", Execution::Idle, &root);
    let _b = add_run_in(&f, "rb", "pb", "claude", Execution::Idle, &root);
    watch::tick(&f.s, 10_000);
    assert!(
        f.s.collision
            .inner
            .lock()
            .unwrap()
            .roots
            .get(&root)
            .is_none_or(|r| r.last_poll_ms == 0),
        "nobody working: no poll"
    );
    set_exec(&f.s, "ra", Execution::Working);
    watch::tick(&f.s, 20_000);
    let first = f.s.collision.inner.lock().unwrap().roots[&root].last_poll_ms;
    assert_eq!(first, 20_000);
    watch::tick(&f.s, 22_000);
    assert_eq!(
        f.s.collision.inner.lock().unwrap().roots[&root].last_poll_ms,
        20_000,
        "inside the 5 s period"
    );
    watch::tick(&f.s, 26_000);
    assert_eq!(
        f.s.collision.inner.lock().unwrap().roots[&root].last_poll_ms,
        26_000
    );
}

// ---- claims ---------------------------------------------------------------------------------

#[tokio::test]
async fn claims_raise_high_at_once_and_have_a_lifecycle() {
    let f = fx("claims");
    let a = add_run(&f, "ra", "pa", "claude", Execution::Working);
    let b = add_run(&f, "rb", "pb", "codex", Execution::Working);
    let r = ok(
        &f.s,
        &user(),
        "collision.claim",
        json!({"run": "ra", "glob": "src/auth/**", "note": "auth rewrite"}),
    )
    .await;
    assert_eq!(r["claim"]["glob"], "src/auth/**");
    assert_eq!(r["claim"]["run"], "ra");
    assert!(r["label"].as_str().unwrap().contains("advisory"));
    // Idempotent for the same run, root and glob.
    let again = ok(
        &f.s,
        &user(),
        "collision.claim",
        json!({"run": "ra", "glob": "src/auth/**"}),
    )
    .await;
    assert_eq!(again["claim"]["id"], r["claim"]["id"]);
    assert_eq!(events(&f.s, "task.collision_claim_added").len(), 1);
    // The owner writing inside its own claim: nothing. A foreign run: high, with one write.
    post(&f, &a, "Edit", "src/auth/login.ts");
    assert!(open(&f.s).is_empty());
    post(&f, &b, "Edit", "src/auth/session.ts");
    let recs = open(&f.s);
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].severity, vc::Severity::High);
    assert!(matches!(&recs[0].paths[0].reason, vc::Reason::Claim { owner, .. } if owner == "ra"));
    assert_eq!(
        events(&f.s, "task.collision_detected")[0].data["reason"],
        "claim"
    );
    // Listing.
    let l = ok(&f.s, &user(), "collision.claims", json!({})).await;
    assert_eq!(l["claims"].as_array().unwrap().len(), 1);
    let l = ok(&f.s, &user(), "collision.claims", json!({"run": "rb"})).await;
    assert!(l["claims"].as_array().unwrap().is_empty());
    // A competing overlapping claim is reported, not refused.
    let c2 = ok(
        &f.s,
        &user(),
        "collision.claim",
        json!({"run": "rb", "glob": "src/auth/login.ts"}),
    )
    .await;
    assert_eq!(c2["conflicts"].as_array().unwrap().len(), 1);
    // Release.
    let rel = ok(
        &f.s,
        &user(),
        "collision.claim_release",
        json!({"run": "rb"}),
    )
    .await;
    assert_eq!(rel["released"].as_array().unwrap().len(), 1);
    let rel = ok(
        &f.s,
        &user(),
        "collision.claim_release",
        json!({"claim": r["claim"]["id"]}),
    )
    .await;
    assert_eq!(rel["released"].as_array().unwrap().len(), 1);
    assert_eq!(events(&f.s, "task.collision_claim_released").len(), 2);
    assert!(
        dispatch(&f.s, &user(), "collision.claim_release", &json!({}))
            .await
            .is_err(),
        "a release must name what it releases"
    );
}

#[tokio::test]
async fn claim_input_is_validated() {
    let f = fx("claimbad");
    let _a = add_run(&f, "ra", "pa", "claude", Execution::Working);
    for glob in ["../etc/*", "", "./"] {
        let r = dispatch(
            &f.s,
            &user(),
            "collision.claim",
            &json!({"run": "ra", "glob": glob}),
        )
        .await;
        assert!(r.is_err(), "glob {glob:?} refused");
    }
    let r = dispatch(&f.s, &user(), "collision.claim", &json!({"glob": "src/**"})).await;
    assert!(r.is_err(), "no run named");
    let r = dispatch(
        &f.s,
        &user(),
        "collision.claim",
        &json!({"run": "nope", "glob": "src/**"}),
    )
    .await;
    assert!(r.is_err(), "unknown run");
    // An absolute path inside the root becomes relative.
    let c = ok(
        &f.s,
        &user(),
        "collision.claim",
        json!({"run": "ra", "glob": format!("{}/src/auth", f.root)}),
    )
    .await;
    assert_eq!(c["claim"]["glob"], "src/auth");
}

#[tokio::test]
async fn an_agent_manages_its_own_claims_only() {
    let f = fx("claimscope");
    let _a = add_run(&f, "ra", "pa", "claude", Execution::Working);
    let _b = add_run(&f, "rb", "pb", "claude", Execution::Working);
    // Own run (implied by the pane token).
    let c = ok(
        &f.s,
        &pane_ctx("pa"),
        "collision.claim",
        json!({"glob": "docs/**"}),
    )
    .await;
    assert_eq!(c["claim"]["run"], "ra");
    // Another run: refused.
    let r = dispatch(
        &f.s,
        &pane_ctx("pa"),
        "collision.claim",
        &json!({"run": "rb", "glob": "x/**"}),
    )
    .await;
    assert!(
        matches!(&r, Err(e) if e.code == ErrorKind::PermissionDenied.code()),
        "{r:?}"
    );
    // Releasing someone else's claim by name releases nothing of theirs.
    let cb = ok(
        &f.s,
        &user(),
        "collision.claim",
        json!({"run": "rb", "glob": "web/**"}),
    )
    .await;
    let rel = ok(
        &f.s,
        &pane_ctx("pa"),
        "collision.claim_release",
        json!({"claim": cb["claim"]["id"]}),
    )
    .await;
    assert!(rel["released"].as_array().unwrap().is_empty());
    // Reads: the claims of the agent's own checkout.
    let l = ok(&f.s, &pane_ctx("pa"), "collision.claims", json!({})).await;
    assert_eq!(l["claims"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn enforced_claims_deny_reported_edit_tools_for_cooperating_adapters_only() {
    let f = fx("enforce");
    let _a = add_run(&f, "ra", "pa", "claude", Execution::Working);
    let _b = add_run(&f, "rb", "pb", "claude", Execution::Working);
    let _c = add_run(&f, "rc", "pc", "codex", Execution::Working);
    ok(
        &f.s,
        &user(),
        "collision.claim",
        json!({"run": "ra", "glob": "src/auth/**"}),
    )
    .await;
    let edit = json!({"tool_name": "Edit", "tool_use_id": "t1", "tool_input": {"file_path": abs(&f, "src/auth/x.ts")}});
    let sig = |pane: &str, harness: &str, event: &str, payload: Value| {
        let s = f.s.clone();
        let (pane, harness, event) = (pane.to_string(), harness.to_string(), event.to_string());
        async move {
            ok(
                &s,
                &pane_ctx(&pane),
                "adapter.signal",
                json!({"harness": harness, "event": event, "payload": payload}),
            )
            .await
        }
    };
    // Off by default: nothing is denied.
    let r = sig("pb", "claude", "PreToolUse", edit.clone()).await;
    assert!(r.get("hook_output").is_none(), "{r}");
    set_config(
        &f.s,
        vk_config::Collision {
            enforce_claims: true,
            ..test_cfg()
        },
    );
    // Claude, foreign claim: deny with the owner named.
    let r = sig("pb", "claude", "PreToolUse", edit.clone()).await;
    let out = &r["hook_output"]["hookSpecificOutput"];
    assert_eq!(out["hookEventName"], "PreToolUse");
    assert_eq!(out["permissionDecision"], "deny");
    assert!(
        out["permissionDecisionReason"]
            .as_str()
            .unwrap()
            .contains("src/auth/**")
    );
    assert_eq!(
        events(&f.s, "task.collision_action")[0].data["action"],
        "claim_denied"
    );
    // The owner is never denied by its own claim; outside the glob and shell commands are fine.
    let r = sig("pa", "claude", "PreToolUse", edit.clone()).await;
    assert!(r.get("hook_output").is_none());
    let other = json!({"tool_name": "Edit", "tool_input": {"file_path": abs(&f, "docs/a.md")}});
    assert!(
        sig("pb", "claude", "PreToolUse", other)
            .await
            .get("hook_output")
            .is_none()
    );
    let bash =
        json!({"tool_name": "Bash", "tool_input": {"command": "sed -i s/a/b/ src/auth/x.ts"}});
    assert!(
        sig("pb", "claude", "PreToolUse", bash)
            .await
            .get("hook_output")
            .is_none()
    );
    // A harness without a verified pre-tool deny (Codex) is a non-cooperating adapter.
    let r = sig("pc", "codex", "PreToolUse", edit).await;
    assert!(r.get("hook_output").is_none(), "{r}");
}

// ---- actions --------------------------------------------------------------------------------

/// Two runs and a high collision between them.
fn collided(f: &Fx) -> (AgentRun, AgentRun, vc::CollisionRec) {
    let a = add_run(f, "ra", "pa", "claude", Execution::Working);
    let b = add_run(f, "rb", "pb", "claude", Execution::Working);
    post(f, &a, "Edit", "src/auth.ts");
    post(f, &b, "Edit", "src/auth.ts");
    let rec = open(&f.s).remove(0);
    (a, b, rec)
}

#[tokio::test]
async fn ignore_drops_the_path_and_stops_tracking_it() {
    let f = fx("ignore");
    let (a, b, rec) = collided(&f);
    let r = ok(
        &f.s,
        &user(),
        "collision.ignore",
        json!({"collision": rec.id, "path": "src/auth.ts"}),
    )
    .await;
    assert_eq!(r["ignore"]["path"], "src/auth.ts");
    assert_eq!(r["ignore"]["root"], f.root);
    assert!(
        open(&f.s).is_empty(),
        "the only path was ignored: the record closed"
    );
    let cleared = events(&f.s, "task.collision_cleared");
    assert_eq!(cleared.len(), 1);
    assert_eq!(cleared[0].data["reason"], "ignored");
    // The record stays readable as ignored history.
    let g = ok(
        &f.s,
        &user(),
        "collision.list",
        json!({"status": "ignored"}),
    )
    .await;
    assert_eq!(g["collisions"].as_array().unwrap().len(), 1);
    assert_eq!(g["collisions"][0]["status"], "ignored");
    // Further writes of the path raise nothing.
    post(&f, &a, "Edit", "src/auth.ts");
    post(&f, &b, "Edit", "src/auth.ts");
    assert!(open(&f.s).is_empty());
    let l = ok(&f.s, &user(), "collision.ignores", json!({})).await;
    assert_eq!(l["ignores"].as_array().unwrap().len(), 1);
    // Unignore: collisions are raised again.
    let id = l["ignores"][0]["id"].as_str().unwrap().to_string();
    let u = ok(&f.s, &user(), "collision.unignore", json!({"ignore": id})).await;
    assert_eq!(u["removed"], true);
    post(&f, &a, "Edit", "src/auth.ts");
    post(&f, &b, "Edit", "src/auth.ts");
    assert_eq!(open(&f.s).len(), 1);
    // Ignoring needs a collision or a root.
    let r = dispatch(&f.s, &user(), "collision.ignore", &json!({"path": "x"})).await;
    assert!(r.is_err());
}

#[tokio::test]
async fn an_ignore_can_expire() {
    let f = fx("ignoreexp");
    let (_a, _b, rec) = collided(&f);
    ok(
        &f.s,
        &user(),
        "collision.ignore",
        json!({"collision": rec.id, "path": "src/auth.ts", "for_secs": 60}),
    )
    .await;
    sweep(&f.s, now_ms() + 61_000);
    let l = ok(&f.s, &user(), "collision.ignores", json!({})).await;
    assert!(l["ignores"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn tell_reaches_native_channels_only_and_never_types_into_a_tui() {
    let f = fx("tell");
    let (_a, _b, rec) = collided(&f);
    // pi in a TUI and a Codex hook run have no native channel.
    let _pi = add_run(&f, "rpi", "ppi", "pi", Execution::Working);
    assert!(steer_channel_of(&f, "rpi").is_err());
    assert!(steer_channel_of(&f, "ra").is_ok());
    let g = ok(&f.s, &user(), "collision.get", json!({"collision": rec.id})).await;
    let steer = g["steer"].as_array().unwrap();
    assert!(
        steer.iter().all(|s| s["channel"] == "hook_context"),
        "{steer:?}"
    );
    let r = ok(
        &f.s,
        &user(),
        "collision.tell",
        json!({"collision": rec.id}),
    )
    .await;
    let results = r["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|x| x["status"] == "queued"));
    assert!(
        r["text"]
            .as_str()
            .unwrap()
            .starts_with("Note: another agent (")
    );
    assert!(r["text"].as_str().unwrap().contains("src/auth.ts"));
    assert!(r["text"].as_str().unwrap().contains("coordinate or avoid"));
    // The text rides the next hook of that run, once.
    let pa = ok(
        &f.s,
        &pane_ctx("pa"),
        "adapter.signal",
        json!({"harness": "claude", "event": "UserPromptSubmit", "payload": {"prompt": "go on"}}),
    )
    .await;
    let ctx = pa["hook_output"]["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(ctx.contains("rb") || ctx.contains("claude"), "{ctx}");
    assert_eq!(
        pa["hook_output"]["hookSpecificOutput"]["hookEventName"],
        "UserPromptSubmit"
    );
    let again = ok(
        &f.s,
        &pane_ctx("pa"),
        "adapter.signal",
        json!({"harness": "claude", "event": "PostToolUse", "payload": {"tool_name": "Bash", "tool_input": {"command": "ls"}}}),
    )
    .await;
    assert!(again.get("hook_output").is_none(), "delivered once");
    let acts: Vec<String> = events(&f.s, "task.collision_action")
        .iter()
        .map(|e| e.data["action"].as_str().unwrap().to_string())
        .collect();
    assert!(
        acts.contains(&"tell".to_string()) && acts.contains(&"tell_delivered".to_string()),
        "{acts:?}"
    );
    // Custom text, a run that is not in the collision, and an unsupported run.
    let r = ok(
        &f.s,
        &user(),
        "collision.tell",
        json!({"collision": rec.id, "runs": ["rb"], "text": "use the shared helper"}),
    )
    .await;
    assert_eq!(r["text"], "use the shared helper");
    let bad = dispatch(
        &f.s,
        &user(),
        "collision.tell",
        &json!({"collision": rec.id, "runs": ["rpi"]}),
    )
    .await;
    assert!(bad.is_err(), "rpi is not part of the collision");
}

fn steer_channel_of(f: &Fx, id: &str) -> Result<act::Channel, &'static str> {
    act::steer_channel(&run_by_id(&f.s, id).unwrap())
}

#[test]
fn steer_channels_by_transport() {
    let mut r = sample_run("r", "p");
    r.harness = "claude".into();
    r.integration = "hooks".into();
    assert_eq!(act::steer_channel(&r), Ok(act::Channel::HookContext));
    r.integration = "headless:rpc".into();
    r.harness = "pi".into();
    assert_eq!(act::steer_channel(&r), Ok(act::Channel::HeadlessSteer));
    r.integration = "headless:app-server".into();
    r.harness = "codex".into();
    assert_eq!(act::steer_channel(&r), Ok(act::Channel::HeadlessSteer));
    r.integration = "headless:acp".into();
    assert!(act::steer_channel(&r).is_err());
    r.integration = "extension".into();
    r.harness = "pi".into();
    assert!(act::steer_channel(&r).unwrap_err().contains("TUI"));
    r.harness = "codex".into();
    r.integration = "hooks".into();
    assert!(act::steer_channel(&r).is_err());
    r.harness = "claude".into();
    r.integration = "hooks".into();
    r.ended_at_ms = Some(1);
    assert_eq!(act::steer_channel(&r), Err("the run has ended"));
}

#[tokio::test]
async fn queued_context_expires() {
    let f = fx("ctxttl");
    let (_a, _b, rec) = collided(&f);
    ok(
        &f.s,
        &user(),
        "collision.tell",
        json!({"collision": rec.id, "runs": ["ra"]}),
    )
    .await;
    // Pretend the message was queued long ago.
    f.s.collision
        .pending_ctx
        .lock()
        .unwrap()
        .get_mut("ra")
        .unwrap()
        .iter_mut()
        .for_each(|(at, _)| *at -= act::CONTEXT_TTL_MS + 1);
    let r = ok(
        &f.s,
        &pane_ctx("pa"),
        "adapter.signal",
        json!({"harness": "claude", "event": "UserPromptSubmit", "payload": {"prompt": "x"}}),
    )
    .await;
    assert!(
        r.get("hook_output").is_none(),
        "a stale notice is not delivered"
    );
}

#[tokio::test]
async fn pause_validates_its_target() {
    let f = fx("pause");
    let (_a, _b, rec) = collided(&f);
    let c = add_run(&f, "rc", "pc", "claude", Execution::Working);
    let r = dispatch(
        &f.s,
        &user(),
        "collision.pause",
        &json!({"collision": rec.id, "run": c.id}),
    )
    .await;
    assert!(r.is_err(), "rc is not part of the collision");
    let r = dispatch(
        &f.s,
        &user(),
        "collision.pause",
        &json!({"collision": "col_nope", "run": "ra"}),
    )
    .await;
    assert!(matches!(&r, Err(e) if e.code == ErrorKind::NotFound.code()));
    end_run(&f.s, "rb");
    // An ended run is gone from the model: it cannot be paused.
    let r = dispatch(
        &f.s,
        &user(),
        "collision.pause",
        &json!({"collision": rec.id, "run": "rb"}),
    )
    .await;
    assert!(r.is_err(), "{r:?}");
}

#[tokio::test]
async fn start_task_plans_from_the_shared_head_without_touching_anything() {
    let f = fx("fresh");
    let root = real_repo(&f);
    let a = add_run_in(&f, "ra", "pa", "claude", Execution::Working, &root);
    let b = add_run_in(&f, "rb", "pb", "claude", Execution::Working, &root);
    {
        let mut c = f.s.core.lock().unwrap();
        let mut r = c.run("ra").cloned().unwrap();
        r.last_message =
            Some("I rewrote the login flow; key AKIAABCD1234EFGH5678 is in the notes".into());
        let mut tx = Tx::new();
        tx.run(r);
        f.s.commit(&mut c, tx).unwrap();
    }
    let post_in = |run: &AgentRun, rel: &str| {
        let p = json!({"tool_name": "Edit", "tool_use_id": "t", "tool_input": {"file_path": format!("{root}/{rel}")}});
        observe(&f.s, run, "PostToolUse", &p);
    };
    post_in(&a, "src/b.txt");
    post_in(&b, "src/b.txt");
    let rec = open(&f.s).remove(0);
    let head = {
        let out = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    let before = std::fs::read_to_string(std::path::Path::new(&root).join("src/b.txt")).unwrap();
    let r = ok(
        &f.s,
        &user(),
        "collision.start_task",
        json!({"collision": rec.id, "dry_run": true}),
    )
    .await;
    assert_eq!(r["created"], false);
    assert_eq!(r["base"], head);
    assert_eq!(r["harness"], "claude");
    let prompt = r["prompt"].as_str().unwrap();
    assert!(prompt.contains("src/b.txt"), "{prompt}");
    assert!(prompt.contains(&head[..12]));
    assert!(prompt.contains("nothing was moved"));
    assert!(
        !prompt.contains("AKIAABCD1234EFGH5678"),
        "excerpts are redacted: {prompt}"
    );
    assert!(
        r["title"]
            .as_str()
            .unwrap()
            .starts_with("Split from shared checkout")
    );
    // Overrides.
    let r = ok(
        &f.s,
        &user(),
        "collision.start_task",
        json!({"collision": rec.id, "dry_run": true, "title": "Auth split", "harness": "codex", "prompt": "do the thing"}),
    )
    .await;
    assert_eq!(
        (
            r["title"].as_str(),
            r["harness"].as_str(),
            r["prompt"].as_str()
        ),
        (Some("Auth split"), Some("codex"), Some("do the thing"))
    );
    // Nothing in the shared checkout moved.
    assert_eq!(
        std::fs::read_to_string(std::path::Path::new(&root).join("src/b.txt")).unwrap(),
        before
    );
    assert_eq!(open(&f.s).len(), 1, "the collision is untouched");
    assert!(run_by_id(&f.s, "ra").is_some() && run_by_id(&f.s, "rb").is_some());
}

#[tokio::test]
async fn start_task_needs_a_commit_to_start_from() {
    let f = fx("freshnogit");
    let (_a, _b, rec) = collided(&f);
    let r = dispatch(
        &f.s,
        &user(),
        "collision.start_task",
        &json!({"collision": rec.id}),
    )
    .await;
    assert!(r.is_err(), "the fake checkout has no HEAD");
}

// ---- housekeeping and restart ---------------------------------------------------------------

#[test]
fn quiet_records_runs_ended_and_claims_are_swept() {
    let f = fx("sweep");
    let (_a, _b, _rec) = collided(&f);
    let now = now_ms();
    assert_eq!(
        sweep(&f.s, now + 1000),
        0,
        "still inside the window, both runs alive"
    );
    // The window passes with no new touch.
    assert_eq!(sweep(&f.s, now + 31 * 60_000), 1);
    assert!(open(&f.s).is_empty());
    let cleared = events(&f.s, "task.collision_cleared");
    assert_eq!(cleared.len(), 1);
    assert_eq!(cleared[0].data["reason"], "quiet");

    // A record whose runs end is cleared at once, and their claims are released.
    let f = fx("sweep2");
    let (_a, _b, _rec) = collided(&f);
    {
        let mut g = f.s.collision.inner.lock().unwrap();
        g.claims.push(vc::Claim {
            id: "clm_x".into(),
            run: "rb".into(),
            root: f.root.clone(),
            glob: "src/**".into(),
            created_ms: 1,
            note: None,
            also: vec![],
        });
    }
    end_run(&f.s, "rb");
    assert_eq!(sweep(&f.s, now_ms()), 1);
    assert_eq!(
        events(&f.s, "task.collision_cleared")[0].data["reason"],
        "runs_ended"
    );
    let rel = events(&f.s, "task.collision_claim_released");
    assert_eq!(rel.len(), 1);
    assert_eq!(rel[0].data["reason"], "run_ended");
    assert!(f.s.collision.inner.lock().unwrap().claims.is_empty());
}

#[test]
fn start_closes_records_a_previous_server_left_open() {
    let f = fx("restart");
    let (_a, _b, rec) = collided(&f);
    // A new server instance on the same state: nothing is open in memory, the store has one.
    let mut stale = rec.clone();
    stale.id = "col_stale".into();
    {
        let mut c = f.s.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.m.put(K_COLLISION, &stale.id, None, &stale);
        f.s.commit(&mut c, tx).unwrap();
    }
    // Simulate the restart: forget the loaded state.
    f.s.collision.inner.lock().unwrap().open.clear();
    f.s.collision.loaded.store(false, Ordering::Release);
    start(&f.s);
    let cleared = events(&f.s, "task.collision_cleared");
    assert!(
        cleared.iter().any(|e| e.data["reason"] == "restart"),
        "{cleared:?}"
    );
    let closed = f.s.with_core(|c| {
        c.store
            .find::<vc::CollisionRec>(K_COLLISION, "col_stale")
            .unwrap()
            .unwrap()
    });
    assert_eq!(closed.status, vc::Status::Cleared);
    assert_eq!(closed.cleared_reason.as_deref(), Some("restart"));
}

#[test]
fn claims_and_ignores_survive_a_restart() {
    let f = fx("persist");
    let a = add_run(&f, "ra", "pa", "claude", Execution::Working);
    let _ = a;
    let claim = vc::Claim {
        id: "clm_p".into(),
        run: "ra".into(),
        root: f.root.clone(),
        glob: "src/**".into(),
        created_ms: 1,
        note: None,
        also: vec![],
    };
    let ig = Ignore {
        id: "ign_p".into(),
        root: f.root.clone(),
        path: "gen/**".into(),
        created_ms: 1,
        expires_ms: None,
        by: "t".into(),
    };
    {
        let mut c = f.s.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.m.put(K_CLAIM, &claim.id, None, &claim);
        tx.m.put(K_IGNORE, &ig.id, None, &ig);
        f.s.commit(&mut c, tx).unwrap();
    }
    f.s.collision.loaded.store(false, Ordering::Release);
    ensure_loaded(&f.s);
    let g = f.s.collision.inner.lock().unwrap();
    assert_eq!(g.claims, vec![claim]);
    assert_eq!(g.ignores, vec![ig]);
}

// ---- API surface ----------------------------------------------------------------------------

#[tokio::test]
async fn list_get_and_status() {
    let f = fx("api");
    let (_a, _b, rec) = collided(&f);
    let l = ok(&f.s, &user(), "collision.list", json!({})).await;
    assert_eq!(l["enabled"], true);
    let items = l["collisions"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], rec.id);
    assert_eq!(items[0]["severity"], "high");
    assert!(
        items[0].get("timeline").is_none(),
        "the list omits timelines"
    );
    assert_eq!(items[0]["run_info"].as_array().unwrap().len(), 2);
    assert!(
        items[0]["headline"]
            .as_str()
            .unwrap()
            .contains("2 agents editing src/auth.ts")
    );
    // Filters.
    let l = ok(&f.s, &user(), "collision.list", json!({"run": "ra"})).await;
    assert_eq!(l["collisions"].as_array().unwrap().len(), 1);
    let l = ok(
        &f.s,
        &user(),
        "collision.list",
        json!({"status": "cleared"}),
    )
    .await;
    assert!(l["collisions"].as_array().unwrap().is_empty());
    assert!(
        dispatch(&f.s, &user(), "collision.list", &json!({"status": "weird"}))
            .await
            .is_err()
    );
    assert!(
        dispatch(&f.s, &user(), "collision.list", &json!({"run": "nope"}))
            .await
            .is_err()
    );
    // One record with its timeline and the steer report.
    let g = ok(&f.s, &user(), "collision.get", json!({"collision": rec.id})).await;
    assert_eq!(g["collision"]["timeline"].as_array().unwrap().len(), 2);
    assert_eq!(g["collision"]["timeline"][0]["path"], "src/auth.ts");
    assert!(
        dispatch(
            &f.s,
            &user(),
            "collision.get",
            &json!({"collision": "nope"})
        )
        .await
        .is_err()
    );
    // Status.
    let st = ok(&f.s, &user(), "collision.status", json!({})).await;
    assert_eq!(st["enabled"], true);
    assert_eq!(st["fs_attribution"], "auto");
    assert_eq!(st["roots"].as_array().unwrap().len(), 1);
    assert_eq!(st["roots"][0]["root"], f.root);
    assert_eq!(st["window_ms"], 1_800_000);
}

#[tokio::test]
async fn closed_records_are_listed_as_history() {
    let f = fx("history");
    let (_a, _b, _rec) = collided(&f);
    sweep(&f.s, now_ms() + 31 * 60_000);
    let l = ok(&f.s, &user(), "collision.list", json!({"status": "all"})).await;
    assert_eq!(l["collisions"].as_array().unwrap().len(), 1);
    assert_eq!(l["collisions"][0]["status"], "cleared");
    assert_eq!(l["collisions"][0]["cleared_reason"], "quiet");
    let l = ok(
        &f.s,
        &user(),
        "collision.list",
        json!({"status": "cleared"}),
    )
    .await;
    assert_eq!(l["collisions"].as_array().unwrap().len(), 1);
    let l = ok(&f.s, &user(), "collision.list", json!({})).await;
    assert!(l["collisions"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn an_agent_sees_only_its_own_collisions_and_cannot_act_on_them() {
    let f = fx("panescope");
    let (_a, _b, rec) = collided(&f);
    let _c = add_run(&f, "rc", "pc", "claude", Execution::Working);
    let own = ok(&f.s, &pane_ctx("pa"), "collision.list", json!({})).await;
    assert_eq!(own["collisions"].as_array().unwrap().len(), 1);
    let other = ok(&f.s, &pane_ctx("pc"), "collision.list", json!({})).await;
    assert!(other["collisions"].as_array().unwrap().is_empty());
    assert!(
        dispatch(
            &f.s,
            &pane_ctx("pc"),
            "collision.get",
            &json!({"collision": rec.id})
        )
        .await
        .is_err()
    );
    assert!(
        dispatch(
            &f.s,
            &pane_ctx("pa"),
            "collision.get",
            &json!({"collision": rec.id})
        )
        .await
        .is_ok()
    );
    for m in PANE_FORBIDDEN {
        let r = dispatch(&f.s, &pane_ctx("pa"), m, &json!({})).await;
        assert!(
            matches!(&r, Err(e) if e.code == ErrorKind::PermissionDenied.code()),
            "{m}: {r:?}"
        );
    }
}

#[test]
fn every_method_is_registered_with_a_shape_and_a_declared_pane_scope() {
    for (m, mutating) in METHODS {
        let scope = pane_scope_of(m);
        assert_eq!(
            scope == PaneScope::Forbidden,
            PANE_FORBIDDEN.contains(m),
            "{m}: forbidden list and pane_scope_of agree"
        );
        assert!(
            crate::api_schema::registry().methods.contains_key(*m),
            "{m} has a shape"
        );
        assert_eq!(crate::session_api::is_mutating(m), *mutating, "{m}");
    }
    // Quoted for the per-method coverage check: every name appears in a test source.
    for m in [
        "collision.list",
        "collision.get",
        "collision.status",
        "collision.ignores",
        "collision.ignore",
        "collision.unignore",
        "collision.pause",
        "collision.tell",
        "collision.start_task",
        "collision.claim",
        "collision.claims",
        "collision.claim_release",
    ] {
        assert!(METHODS.iter().any(|(n, _)| *n == m));
    }
}

#[test]
fn task_get_lists_the_open_collisions_of_a_task() {
    let f = fx("taskget");
    let (a, _b, _rec) = collided(&f);
    {
        let mut c = f.s.core.lock().unwrap();
        let mut r = c.run("ra").cloned().unwrap();
        r.task = Some("task-1".into());
        let mut tx = Tx::new();
        tx.run(r);
        f.s.commit(&mut c, tx).unwrap();
    }
    let _ = a;
    let task = Task {
        id: "task-1".into(),
        handle: "k1".into(),
        title: "t".into(),
        slug: "t".into(),
        workspace: None,
        repo_root: f.root.clone(),
        worktree_path: None,
        branch: None,
        base_ref: None,
        port_range: None,
        status: "active".into(),
        setup_status: None,
        created_at_ms: 0,
        ownership: Default::default(),
        owner_machine: String::new(),
        intent_revision: None,
        priority: None,
        rev: 0,
        review_label: None,
        effort: None,
        isolation: Default::default(),
        checkout: None,
    };
    let v = for_task(&f.s, &task);
    assert_eq!(v.len(), 1);
    assert_eq!(v[0]["severity"], "high");
    let other = Task {
        id: "task-2".into(),
        ..task
    };
    assert!(for_task(&f.s, &other).is_empty());
}

#[tokio::test]
async fn forget_removes_the_records_and_claims_of_the_runs_in_scope() {
    let f = fx("forget");
    let (_a, _b, _rec) = collided(&f);
    ok(
        &f.s,
        &user(),
        "collision.claim",
        json!({"run": "ra", "glob": "src/**"}),
    )
    .await;
    // The record closes (history); the claim of a live run stays until released.
    sweep(&f.s, now_ms() + 31 * 60_000);
    assert!(open(&f.s).is_empty());
    // Out of scope: nothing.
    assert_eq!(forget(&f.s, &|run, _| run == "zzz", false), 0);
    // A dry run only counts: one record and one claim.
    assert_eq!(forget(&f.s, &|run, _| run == "ra", true), 2);
    let l = ok(&f.s, &user(), "collision.list", json!({"status": "all"})).await;
    assert_eq!(l["collisions"].as_array().unwrap().len(), 1);
    assert_eq!(forget(&f.s, &|run, _| run == "ra", false), 2);
    let l = ok(&f.s, &user(), "collision.list", json!({"status": "all"})).await;
    assert!(l["collisions"].as_array().unwrap().is_empty());
    let c = ok(&f.s, &user(), "collision.claims", json!({})).await;
    assert!(c["claims"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn pre_tool_claims_deny_foreign_edits_when_enforced() {
    let f = fx("enforcegate");
    let _a = add_run(&f, "ra", "pa", "claude", Execution::Working);
    let _b = add_run(&f, "rb", "pb", "claude", Execution::Working);
    ok(
        &f.s,
        &user(),
        "collision.claim",
        json!({"run": "ra", "glob": "src/auth/**"}),
    )
    .await;
    let edit = json!({"tool_name": "Write", "tool_input": {"file_path": abs(&f, "src/auth/x.ts")}});
    assert!(
        pre_tool_claim(&f.s, "pb", &edit).is_none(),
        "off by default"
    );
    set_config(
        &f.s,
        vk_config::Collision {
            enforce_claims: true,
            ..test_cfg()
        },
    );
    let d = pre_tool_claim(&f.s, "pb", &edit).expect("denied");
    assert_eq!(d["hookSpecificOutput"]["permissionDecision"], "deny");
    assert!(pre_tool_claim(&f.s, "pa", &edit).is_none(), "the owner");
}

#[tokio::test]
async fn adapter_gate_needs_a_pane_token_and_passes_plain_signals_through() {
    let f = fx("gatebasics");
    let _a = add_run(&f, "ra", "pa", "claude", Execution::Working);
    let p = json!({"harness": "claude", "event": "PostToolUse", "payload": {}});
    let r = dispatch(&f.s, &user(), "adapter.gate", &p).await;
    assert!(
        matches!(&r, Err(e) if e.code == ErrorKind::PermissionDenied.code()),
        "{r:?}"
    );
    // An event that asks nothing opens no interaction and leaves the harness to decide.
    let r = ok(&f.s, &pane_ctx("pa"), "adapter.gate", p).await;
    assert_eq!(r, json!({"decision": null}));
}

fn put_task_claim(f: &Fx, id: &str, task: &str, glob: &str) {
    let claim = vk_orchestrate::merge::Claim {
        id: id.into(),
        task: task.into(),
        glob: glob.into(),
        note: None,
        created_at_ms: 1,
    };
    let mut c = f.s.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.m.put("orch_claim", id, Some(id), &claim);
    f.s.commit(&mut c, tx).unwrap();
}

fn set_task(s: &Server, run: &str, task: &str) {
    let mut c = s.core.lock().unwrap();
    let mut r = c.run(run).cloned().unwrap();
    r.task = Some(task.into());
    let mut tx = Tx::new();
    tx.run(r);
    s.commit(&mut c, tx).unwrap();
}

#[test]
fn a_tasks_claim_binds_the_runs_of_other_tasks_in_the_same_checkout() {
    let f = fx("taskclaim");
    let a = add_run(&f, "ra", "pa", "claude", Execution::Working);
    let b = add_run(&f, "rb", "pb", "claude", Execution::Working);
    let c = add_run(&f, "rc", "pc", "claude", Execution::Working);
    // ra and rc work for task-1 (which claimed src/**); rb is another task's (or nobody's) run.
    set_task(&f.s, "ra", "task-1");
    set_task(&f.s, "rc", "task-1");
    put_task_claim(&f, "c-1", "task-1", "src/**");
    let eff = effective_claims(&f.s, Some(&f.root));
    assert_eq!(
        eff.len(),
        1,
        "one claim per checkout, owned by every run of the task"
    );
    let mut owners: Vec<&str> = eff[0].owners().collect();
    owners.sort_unstable();
    assert_eq!(owners, ["ra", "rc"]);
    // The task's own runs write inside it: nothing.
    post(&f, &a, "Edit", "src/x.ts");
    post(&f, &c, "Edit", "src/y.ts");
    assert!(
        open(&f.s).iter().all(|r| r.severity < vc::Severity::High),
        "{:?}",
        open(&f.s)
    );
    // Another task's run: high at once, naming the owner.
    post(&f, &b, "Edit", "src/z.ts");
    let high: Vec<vc::CollisionRec> = open(&f.s)
        .into_iter()
        .filter(|r| r.severity == vc::Severity::High)
        .collect();
    assert_eq!(high.len(), 1);
    assert!(
        matches!(&high[0].paths.iter().find(|p| p.path == "src/z.ts").unwrap().reason, vc::Reason::Claim { owner, claim, .. } if owner == "ra" && claim == "c-1")
    );
    // A claim whose task has no live run in this checkout binds nothing here.
    put_task_claim(&f, "c-2", "task-9", "docs/**");
    assert_eq!(effective_claims(&f.s, Some(&f.root)).len(), 1);
}

#[tokio::test]
async fn task_claim_of_a_run_without_a_task_is_a_run_claim() {
    let f = fx("claimhook");
    let _a = add_run(&f, "ra", "pa", "claude", Execution::Working);
    let _b = add_run(&f, "rb", "pb", "claude", Execution::Working);
    // Merge orchestration is off, yet an untasked run can claim: the hook takes it first.
    let r = ok(
        &f.s,
        &user(),
        "task.claim",
        json!({"run": "ra", "glob": "src/auth/**"}),
    )
    .await;
    let id = r["claim"]["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("clm_"), "{r}");
    assert_eq!(r["claim"]["kind"], "run");
    assert_eq!(r["claim"]["task"], Value::Null);
    // The pane token of ra may claim for itself, without naming a run.
    let r2 = ok(
        &f.s,
        &pane_ctx("pa"),
        "task.claim",
        json!({"glob": "docs/**"}),
    )
    .await;
    assert_eq!(r2["claim"]["run"], "ra");
    // It is listed with the task claims and removable with `task.claim.remove`.
    let l = ok(&f.s, &user(), "task.claim.list", json!({})).await;
    let ids: Vec<&str> = l["claims"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&id.as_str()), "{l}");
    // Another run's write inside it is a high collision.
    let b = run_by_id(&f.s, "rb").unwrap();
    post(&f, &b, "Edit", "src/auth/x.ts");
    assert_eq!(open(&f.s)[0].severity, vc::Severity::High);
    let rm = ok(&f.s, &user(), "task.claim.remove", json!({"claim": id})).await;
    assert_eq!(rm["removed"], id);
    assert!(
        dispatch(
            &f.s,
            &user(),
            "task.claim.remove",
            &json!({"claim": "clm_nope"})
        )
        .await
        .is_err()
    );
    // A claim for a named task is merge orchestration's: refused while it is off, not ours.
    let t = dispatch(
        &f.s,
        &user(),
        "task.claim",
        &json!({"task": "k9", "glob": "x/**"}),
    )
    .await;
    assert!(t.is_err());
    // `collision.claims` shows run claims and the task claims that bind the checkout.
    put_task_claim(&f, "c-7", "task-7", "web/**");
    set_task(&f.s, "rb", "task-7");
    let all = ok(&f.s, &user(), "collision.claims", json!({})).await;
    let kinds: Vec<&str> = all["claims"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"run") && kinds.contains(&"task"), "{all}");
}
