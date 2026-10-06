//! The collision rules (05 §10): same file, same directory, read-then-edited, claims, attribution,
//! windows and the record lifecycle. Pure: no process, no file system beyond temp dirs.

use vk_tasks::collision::*;

const MIN: i64 = 60_000;

fn rules() -> Rules {
    Rules::default()
}

fn w(run: &str, path: &str, at: i64) -> Touch {
    Touch::write(run, path, at, Source::Adapter, "modify")
}

#[test]
fn one_run_editing_alone_is_never_a_collision() {
    let mut t = Tracker::new();
    assert!(t.record(&rules(), &[], w("a", "src/x.rs", 0)).is_empty());
    assert!(t.record(&rules(), &[], w("a", "src/x.rs", 1000)).is_empty());
    assert!(t.record(&rules(), &[], w("a", "src/y.rs", 2000)).is_empty());
}

#[test]
fn two_runs_on_one_file_is_high_with_both_runs() {
    let mut t = Tracker::new();
    assert!(t.record(&rules(), &[], w("a", "src/auth.ts", 0)).is_empty());
    let f = t.record(&rules(), &[], w("b", "src/auth.ts", 1000));
    let hit = f
        .iter()
        .find(|f| f.reason == Reason::SameFile)
        .expect("same file");
    assert_eq!(hit.severity, Severity::High);
    assert_eq!(hit.runs, vec!["a".to_string(), "b".to_string()]);
    assert_eq!(hit.paths, vec!["src/auth.ts".to_string()]);
    assert!(!hit.ambiguous);
}

#[test]
fn the_window_forgets_old_touches() {
    let mut t = Tracker::new();
    t.record(&rules(), &[], w("a", "f.rs", 0));
    let f = t.record(&rules(), &[], w("b", "f.rs", 31 * MIN));
    assert!(
        f.is_empty(),
        "31 minutes later is outside the 30 minute window"
    );
    let mut t = Tracker::new();
    t.record(&rules(), &[], w("a", "f.rs", 0));
    let f = t.record(&rules(), &[], w("b", "f.rs", 29 * MIN));
    assert_eq!(f.len(), 1);
}

#[test]
fn same_directory_is_low_and_root_files_do_not_count() {
    let mut t = Tracker::new();
    t.record(&rules(), &[], w("a", "src/auth/login.ts", 0));
    let f = t.record(&rules(), &[], w("b", "src/auth/logout.ts", 1000));
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].severity, Severity::Low);
    assert_eq!(
        f[0].reason,
        Reason::SameDir {
            dir: "src/auth".into()
        }
    );
    assert_eq!(f[0].paths.len(), 2);
    // Files at the repo root share no module.
    let mut t = Tracker::new();
    t.record(&rules(), &[], w("a", "README.md", 0));
    assert!(
        t.record(&rules(), &[], w("b", "Cargo.toml", 1000))
            .is_empty()
    );
}

#[test]
fn directory_depth_is_configurable_and_zero_turns_the_rule_off() {
    let deep = Rules {
        dir_depth: 1,
        ..rules()
    };
    let mut t = Tracker::new();
    t.record(&deep, &[], w("a", "src/x/a.rs", 0));
    let f = t.record(&deep, &[], w("b", "src/y/b.rs", 1000));
    assert_eq!(f[0].reason, Reason::SameDir { dir: "src".into() });
    let off = Rules {
        dir_depth: 0,
        ..rules()
    };
    let mut t = Tracker::new();
    t.record(&off, &[], w("a", "src/x/a.rs", 0));
    assert!(t.record(&off, &[], w("b", "src/x/b.rs", 1000)).is_empty());
    assert_eq!(dir_key("a/b/c/d.rs", 2).as_deref(), Some("a/b"));
    assert_eq!(dir_key("a/d.rs", 2).as_deref(), Some("a"));
    assert_eq!(dir_key("d.rs", 2), None);
}

#[test]
fn editing_a_file_another_run_read_is_medium() {
    let mut t = Tracker::new();
    t.record(&rules(), &[], Touch::read("claude", "api/auth.ts", 0));
    let f = t.record(&rules(), &[], w("codex", "api/auth.ts", 5 * MIN));
    let m = f
        .iter()
        .find(|f| f.severity == Severity::Medium)
        .expect("medium");
    assert_eq!(
        m.reason,
        Reason::ReadThenEdited {
            editor: Some("codex".into()),
            reader: "claude".into()
        }
    );
    assert_eq!(m.runs, vec!["claude".to_string(), "codex".to_string()]);
    // A read older than ten minutes no longer counts.
    let mut t = Tracker::new();
    t.record(&rules(), &[], Touch::read("claude", "api/auth.ts", 0));
    let f = t.record(&rules(), &[], w("codex", "api/auth.ts", 11 * MIN));
    assert!(f.iter().all(|f| f.severity != Severity::Medium));
    // Reading your own edit is nothing.
    let mut t = Tracker::new();
    t.record(&rules(), &[], Touch::read("a", "x.rs", 0));
    assert!(t.record(&rules(), &[], w("a", "x.rs", 1000)).is_empty());
}

#[test]
fn a_write_inside_a_foreign_claim_is_high_at_once() {
    let claim = Claim {
        id: "c1".into(),
        run: "a".into(),
        root: "/r".into(),
        glob: "src/auth/**".into(),
        created_ms: 0,
        note: None,
    };
    let mut t = Tracker::new();
    let f = t.record(
        &rules(),
        std::slice::from_ref(&claim),
        w("b", "src/auth/x.ts", 10),
    );
    assert_eq!(f.len(), 1, "a single write, no second writer needed");
    assert_eq!(f[0].severity, Severity::High);
    assert!(matches!(&f[0].reason, Reason::Claim { owner, .. } if owner == "a"));
    // The owner writing inside its own claim, and anyone writing outside it: nothing.
    assert!(
        t.record(
            &rules(),
            std::slice::from_ref(&claim),
            w("a", "src/auth/y.ts", 20)
        )
        .iter()
        .all(|f| !matches!(f.reason, Reason::Claim { .. }))
    );
    assert!(
        t.record(
            &rules(),
            std::slice::from_ref(&claim),
            w("b", "docs/x.md", 30)
        )
        .is_empty()
    );
}

#[test]
fn ambiguous_attribution_is_flagged_and_never_collides_with_itself() {
    let mut t = Tracker::new();
    // Two runs were working: we can't say which one wrote it.
    let amb = Touch::ambiguous(vec!["a".into(), "b".into()], "f.rs", 0, Source::Watcher);
    assert!(t.record(&rules(), &[], amb.clone()).is_empty());
    // Another ambiguous touch of the same candidates is no evidence of two writers.
    assert!(
        t.record(&rules(), &[], Touch { at_ms: 1000, ..amb })
            .is_empty()
    );
    // A certain write by c meets an ambiguous one that cannot be c: a collision, flagged.
    let f = t.record(&rules(), &[], w("c", "f.rs", 2000));
    let hit = f
        .iter()
        .find(|f| f.reason == Reason::SameFile)
        .expect("collision");
    assert!(hit.ambiguous);
    assert_eq!(
        hit.runs,
        vec!["a".to_string(), "b".to_string(), "c".to_string()]
    );
}

#[test]
fn an_ambiguous_write_that_could_be_the_same_run_is_not_a_collision() {
    let mut t = Tracker::new();
    t.record(&rules(), &[], w("a", "f.rs", 0));
    let amb = Touch::ambiguous(vec!["a".into(), "b".into()], "f.rs", 1000, Source::Git);
    let f = t.record(&rules(), &[], amb);
    // It may be a's second edit; the other candidate b cannot be proven.
    assert!(f.iter().all(|f| f.reason != Reason::SameFile) || f.iter().any(|f| f.ambiguous));
}

#[test]
fn attribution_prefers_in_flight_then_fd_then_working() {
    let runs = vec![
        RunView {
            run: "a".into(),
            working: true,
            in_flight: vec!["src/x.rs".into()],
        },
        RunView {
            run: "b".into(),
            working: true,
            in_flight: vec![],
        },
    ];
    assert_eq!(
        attribute("src/x.rs", &runs, &[]),
        Attribution::Run("a".into())
    );
    // No in-flight report for this path: both are working, so it is ambiguous...
    assert_eq!(
        attribute("src/y.rs", &runs, &[]),
        Attribution::Ambiguous(vec!["a".into(), "b".into()])
    );
    // ... unless open-file sampling named the writer.
    assert_eq!(
        attribute("src/y.rs", &runs, &["b".to_string()]),
        Attribution::Run("b".into())
    );
    // A writer that is not a known run in this directory is ignored.
    assert_eq!(
        attribute("src/y.rs", &runs, &["zzz".to_string()]),
        Attribution::Ambiguous(vec!["a".into(), "b".into()])
    );
    // Nobody working: not an agent's change.
    let idle = vec![RunView {
        run: "a".into(),
        working: false,
        in_flight: vec![],
    }];
    assert_eq!(attribute("src/y.rs", &idle, &[]), Attribution::None);
    // One working run owns it.
    let one = vec![
        RunView {
            run: "a".into(),
            working: true,
            in_flight: vec![],
        },
        RunView {
            run: "b".into(),
            working: false,
            in_flight: vec![],
        },
    ];
    assert_eq!(attribute("q", &one, &[]), Attribution::Run("a".into()));
}

#[test]
fn records_merge_by_run_set_and_keep_the_strongest_hit_per_path() {
    let mut t = Tracker::new();
    t.record(&rules(), &[], w("a", "src/a.rs", 0));
    let f1 = t.record(&rules(), &[], w("b", "src/a.rs", 1000));
    let mut rec = CollisionRec::new("col1", "/r", 0);
    let f = f1.iter().find(|f| f.severity == Severity::High).unwrap();
    assert!(rec.accepts("/r", f));
    let m = rec.merge(f);
    assert!(m.created && m.changed());
    assert_eq!(rec.severity, Severity::High);
    // A finding on another file by the same pair merges into the same record.
    t.record(&rules(), &[], w("a", "src/b.rs", 2000));
    let f2 = t.record(&rules(), &[], w("b", "src/b.rs", 3000));
    let high = f2.iter().find(|f| f.severity == Severity::High).unwrap();
    assert!(rec.accepts("/r", high));
    let m = rec.merge(high);
    assert_eq!(m.new_paths, vec!["src/b.rs".to_string()]);
    assert!(!m.created);
    assert_eq!(rec.paths.len(), 2);
    // Another root never merges.
    assert!(!rec.accepts("/other", high));
    // A third run joining a pair record (superset of the run set) merges too.
    let f3 = Finding {
        severity: Severity::High,
        reason: Reason::SameFile,
        paths: vec!["src/a.rs".into()],
        runs: vec!["a".into(), "b".into(), "c".into()],
        ambiguous: false,
        at_ms: 4000,
    };
    assert!(rec.accepts("/r", &f3));
    let m = rec.merge(&f3);
    assert_eq!(m.new_runs, vec!["c".to_string()]);
    // A disjoint pair does not.
    let f4 = Finding {
        runs: vec!["x".into(), "y".into()],
        ..f3.clone()
    };
    assert!(!rec.accepts("/r", &f4));
}

#[test]
fn severity_only_goes_up_and_a_raise_is_reported() {
    let mut rec = CollisionRec::new("c", "/r", 0);
    let low = Finding {
        severity: Severity::Low,
        reason: Reason::SameDir { dir: "src".into() },
        paths: vec!["src/a".into(), "src/b".into()],
        runs: vec!["a".into(), "b".into()],
        ambiguous: false,
        at_ms: 1,
    };
    assert!(rec.merge(&low).created);
    let high = Finding {
        severity: Severity::High,
        reason: Reason::SameFile,
        paths: vec!["src/a".into()],
        runs: vec!["a".into(), "b".into()],
        ambiguous: false,
        at_ms: 2,
    };
    let m = rec.merge(&high);
    assert!(m.severity_raised && !m.created);
    assert_eq!(rec.severity, Severity::High);
    // A later low hit on the same path leaves it high.
    rec.merge(&low);
    assert_eq!(rec.severity, Severity::High);
    assert_eq!(rec.headline_paths(1), vec!["src/a".to_string()]);
}

#[test]
fn ignoring_every_path_closes_the_record() {
    let mut rec = CollisionRec::new("c", "/r", 0);
    rec.merge(&Finding {
        severity: Severity::High,
        reason: Reason::SameFile,
        paths: vec!["a".into(), "b".into()],
        runs: vec!["x".into(), "y".into()],
        ambiguous: false,
        at_ms: 1,
    });
    assert!(rec.ignore_path("a"));
    assert_eq!(rec.status, Status::Open);
    assert!(!rec.ignore_path("a"), "already gone");
    assert!(rec.ignore_path("b"));
    assert_eq!(rec.status, Status::Ignored);
}

#[test]
fn a_path_set_notifies_once_per_window() {
    let mut rec = CollisionRec::new("c", "/r", 0);
    rec.merge(&Finding {
        severity: Severity::High,
        reason: Reason::SameFile,
        paths: vec!["a".into()],
        runs: vec!["x".into(), "y".into()],
        ambiguous: false,
        at_ms: 1,
    });
    let win = 30 * MIN;
    assert!(rec.should_notify(0, win));
    assert!(!rec.should_notify(MIN, win), "same path set, same window");
    // A new path changes the set: announced again.
    rec.merge(&Finding {
        severity: Severity::High,
        reason: Reason::SameFile,
        paths: vec!["b".into()],
        runs: vec!["x".into(), "y".into()],
        ambiguous: false,
        at_ms: 2,
    });
    assert!(rec.should_notify(2 * MIN, win));
    assert!(!rec.should_notify(3 * MIN, win));
    // After the window the same set is announced again.
    assert!(rec.should_notify(40 * MIN, win));
    assert_ne!(
        path_set_key(&["a".into()], Severity::Medium),
        path_set_key(&["a".into()], Severity::High)
    );
    assert_eq!(
        path_set_key(&["a".into(), "b".into()], Severity::High),
        path_set_key(&["b".into(), "a".into()], Severity::High)
    );
}

#[test]
fn timeline_is_bounded_and_forget_helpers_work() {
    let mut rec = CollisionRec::new("c", "/r", 0);
    for i in 0..(TIMELINE_MAX as i64 + 20) {
        rec.note(&w("a", "f", i));
    }
    assert_eq!(rec.timeline.len(), TIMELINE_MAX);
    assert_eq!(rec.timeline.first().unwrap().at_ms, 20);
    let mut t = Tracker::new();
    t.record(&rules(), &[], w("a", "f", 0));
    t.record(&rules(), &[], w("b", "g", 1));
    t.forget_path("f");
    assert_eq!(t.len(), 1);
    t.forget_run("b");
    assert!(t.is_empty());
}

#[test]
fn the_touch_cap_drops_the_oldest() {
    let r = Rules {
        max_touches: 3,
        ..rules()
    };
    let mut t = Tracker::new();
    for i in 0..10 {
        t.record(&r, &[], w("a", &format!("f{i}"), i));
    }
    assert_eq!(t.len(), 3);
    assert_eq!(t.touches().next().unwrap().path, "f7");
}
