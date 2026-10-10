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
        also: vec![],
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
    // A certain write by c meets an ambiguous one that cannot be c: a possible collision,
    // flagged, and only medium.
    let f = t.record(&rules(), &[], w("c", "f.rs", 2000));
    let hit = f
        .iter()
        .find(|f| f.reason == Reason::SameFile)
        .expect("collision");
    assert!(hit.ambiguous);
    assert_eq!(hit.severity, Severity::Medium);
    assert_eq!(
        hit.runs,
        vec!["a".to_string(), "b".to_string(), "c".to_string()]
    );
}

#[test]
fn an_ambiguous_write_that_could_be_the_same_run_is_not_a_collision() {
    let mut t = Tracker::new();
    t.record(&rules(), &[], w("a", "f.rs", 0));
    // Most likely a's own formatter or build: the change is explained by a.
    let amb = Touch::ambiguous(vec!["a".into(), "b".into()], "f.rs", 1000, Source::Git);
    let f = t.record(&rules(), &[], amb);
    assert!(f.iter().all(|f| f.reason != Reason::SameFile), "{f:?}");
    // The same the other way round: a guess first, then a's report.
    let mut t = Tracker::new();
    t.record(
        &rules(),
        &[],
        Touch::ambiguous(vec!["a".into(), "b".into()], "g.rs", 0, Source::Watcher),
    );
    let f = t.record(&rules(), &[], w("a", "g.rs", 1000));
    assert!(f.iter().all(|f| f.reason != Reason::SameFile), "{f:?}");
}

#[test]
fn an_inferred_writer_is_at_most_medium() {
    let mut t = Tracker::new();
    t.record(&rules(), &[], w("a", "f.rs", 0));
    // b was the only run working when f.rs changed again.
    let f = t.record(
        &rules(),
        &[],
        Touch::inferred("b", "f.rs", 1000, Source::Git, "modify"),
    );
    let hit = f
        .iter()
        .find(|f| f.reason == Reason::SameFile)
        .expect("collision");
    assert_eq!(hit.severity, Severity::Medium);
    assert!(!hit.ambiguous, "one run is named");
    // Two inferred writers are a guess on both sides: still medium, never high.
    let mut t = Tracker::new();
    t.record(
        &rules(),
        &[],
        Touch::inferred("a", "f.rs", 0, Source::Git, "modify"),
    );
    let f = t.record(
        &rules(),
        &[],
        Touch::inferred("b", "f.rs", 1000, Source::Git, "modify"),
    );
    assert!(f.iter().all(|f| f.severity < Severity::High), "{f:?}");
    // A guessed write never makes a same-directory hint.
    let mut t = Tracker::new();
    t.record(&rules(), &[], w("a", "src/auth/a.rs", 0));
    let f = t.record(
        &rules(),
        &[],
        Touch::inferred("b", "src/auth/b.rs", 1000, Source::Watcher, "modify"),
    );
    assert!(f.is_empty(), "{f:?}");
    // A guessed edit of a file another run read is low.
    let mut t = Tracker::new();
    t.record(&rules(), &[], Touch::read("a", "x.rs", 0));
    let f = t.record(
        &rules(),
        &[],
        Touch::inferred("b", "x.rs", 1000, Source::Git, "modify"),
    );
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].severity, Severity::Low);
}

#[test]
fn a_guessed_write_inside_a_foreign_claim_is_medium() {
    let claim = Claim {
        id: "c1".into(),
        run: "a".into(),
        root: "/r".into(),
        glob: "src/**".into(),
        created_ms: 0,
        note: None,
        also: vec![],
    };
    let mut t = Tracker::new();
    let f = t.record(
        &rules(),
        std::slice::from_ref(&claim),
        Touch::inferred("b", "src/x.rs", 10, Source::Watcher, "modify"),
    );
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].severity, Severity::Medium);
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
    // One working run is the likely writer: inferred, not reported.
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
    assert_eq!(attribute("q", &one, &[]), Attribution::Inferred("a".into()));
    // With more runs working than MAX_CANDIDATES, "one of them" says nothing.
    let many: Vec<RunView> = (0..=MAX_CANDIDATES)
        .map(|i| RunView {
            run: format!("r{i}"),
            working: true,
            in_flight: vec![],
        })
        .collect();
    assert_eq!(attribute("q", &many, &[]), Attribution::None);
}

#[test]
fn records_merge_by_checkout_and_keep_the_strongest_hit_per_path() {
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
    // A disjoint pair in the same checkout merges as well: one record per checkout.
    let f4 = Finding {
        runs: vec!["x".into(), "y".into()],
        ..f3.clone()
    };
    assert!(rec.accepts("/r", &f4));
}

#[test]
fn paths_decay_one_by_one_and_take_their_runs_with_them() {
    let mut rec = CollisionRec::new("c", "/r", 0);
    rec.merge(&Finding {
        severity: Severity::High,
        reason: Reason::SameFile,
        paths: vec!["old.rs".into()],
        runs: vec!["a".into(), "b".into()],
        ambiguous: false,
        at_ms: 0,
    });
    rec.merge(&Finding {
        severity: Severity::Medium,
        reason: Reason::SameFile,
        paths: vec!["new.rs".into()],
        runs: vec!["c".into(), "d".into()],
        ambiguous: true,
        at_ms: 20 * MIN,
    });
    assert_eq!(rec.runs.len(), 4);
    assert!(!rec.expire(25 * MIN, 30 * MIN), "nothing is old yet");
    assert!(rec.expire(31 * MIN, 30 * MIN));
    assert_eq!(rec.paths.len(), 1);
    assert_eq!(rec.paths[0].path, "new.rs");
    assert_eq!(rec.runs, vec!["c".to_string(), "d".to_string()]);
    assert_eq!(rec.severity, Severity::Medium);
    assert!(rec.ambiguous);
    assert!(!rec.notable(), "a guess is never announced");
    assert!(rec.expire(51 * MIN, 30 * MIN));
    assert!(rec.paths.is_empty());
}

#[test]
fn a_record_keeps_at_most_paths_max_paths() {
    let mut rec = CollisionRec::new("c", "/r", 0);
    for i in 0..(PATHS_MAX as i64 + 10) {
        rec.merge(&Finding {
            severity: Severity::Low,
            reason: Reason::SameDir { dir: "src".into() },
            paths: vec![format!("src/f{i}.rs")],
            runs: vec!["a".into(), "b".into()],
            ambiguous: false,
            at_ms: i,
        });
    }
    let m = rec.merge(&Finding {
        severity: Severity::High,
        reason: Reason::SameFile,
        paths: vec!["src/hot.rs".into()],
        runs: vec!["a".into(), "b".into()],
        ambiguous: false,
        at_ms: 1,
    });
    assert_eq!(rec.paths.len(), PATHS_MAX);
    assert_eq!(m.new_paths, vec!["src/hot.rs".to_string()]);
    assert!(rec.paths.iter().any(|h| h.path == "src/hot.rs"));
    assert!(
        !rec.paths.iter().any(|h| h.path == "src/f0.rs"),
        "oldest low went first"
    );
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

#[test]
fn a_weaker_hit_never_adds_its_runs_to_a_stronger_path() {
    let mut rec = CollisionRec::new("c", "/r", 0);
    rec.merge(&Finding {
        severity: Severity::High,
        reason: Reason::SameFile,
        paths: vec!["src/a.rs".into()],
        runs: vec!["a".into(), "b".into()],
        ambiguous: false,
        at_ms: 1,
    });
    // c wrote a neighbour: the same-directory hint names src/a.rs too.
    let m = rec.merge(&Finding {
        severity: Severity::Low,
        reason: Reason::SameDir { dir: "src".into() },
        paths: vec!["src/b.rs".into(), "src/a.rs".into()],
        runs: vec!["a".into(), "c".into()],
        ambiguous: false,
        at_ms: 2,
    });
    let a = rec.paths.iter().find(|h| h.path == "src/a.rs").unwrap();
    assert_eq!(a.runs, vec!["a".to_string(), "b".to_string()]);
    assert_eq!(a.severity, Severity::High);
    assert_eq!(
        m.new_runs,
        vec!["c".to_string()],
        "c is in the record via src/b.rs"
    );
    // A guess on the same file leaves the reported pair alone.
    rec.merge(&Finding {
        severity: Severity::Medium,
        reason: Reason::SameFile,
        paths: vec!["src/a.rs".into()],
        runs: vec!["a".into(), "d".into(), "e".into()],
        ambiguous: true,
        at_ms: 3,
    });
    let a = rec.paths.iter().find(|h| h.path == "src/a.rs").unwrap();
    assert_eq!(a.runs, vec!["a".to_string(), "b".to_string()]);
    assert!(!a.ambiguous);
}

#[test]
fn a_guess_joining_a_notified_record_is_not_announced() {
    let mut rec = CollisionRec::new("c", "/r", 0);
    let high = Finding {
        severity: Severity::High,
        reason: Reason::SameFile,
        paths: vec!["a.rs".into()],
        runs: vec!["x".into(), "y".into()],
        ambiguous: false,
        at_ms: 1,
    };
    assert!(high.notable());
    rec.merge(&high);
    assert!(rec.should_notify(0, 30 * MIN));
    let guess = Finding {
        severity: Severity::Medium,
        reason: Reason::SameFile,
        paths: vec!["b.rs".into()],
        runs: vec!["x".into(), "z".into()],
        ambiguous: true,
        at_ms: 2,
    };
    assert!(!guess.notable());
    rec.merge(&guess);
    assert!(
        !rec.should_notify(MIN, 30 * MIN),
        "the notable path set did not change"
    );
}
