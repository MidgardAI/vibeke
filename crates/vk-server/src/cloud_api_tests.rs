//! Unit tests of `cloud_api.rs`: the adopt checks and session broker names.

use super::*;
use std::collections::HashMap;

#[test]
fn adopt_needs_its_own_worktree_on_the_branch() {
    let repo = tempfile::tempdir().unwrap();
    let wt = tempfile::tempdir().unwrap();
    let task = |branch: &str, path: &std::path::Path, checkout: Option<&str>| {
        let mut t = json!({"branch": branch, "worktree_path": path});
        if let Some(c) = checkout {
            t["checkout"] = json!(c);
        }
        t
    };
    assert!(adopt_checkout_ok(
        &task("vk/b", wt.path(), Some("worktree")),
        repo.path(),
        "vk/b"
    ));
    assert!(adopt_checkout_ok(
        &task("vk/b", wt.path(), None),
        repo.path(),
        "vk/b"
    ));
    // The main checkout, another branch, or no checkout at all is refused.
    assert!(!adopt_checkout_ok(
        &task("vk/b", repo.path(), None),
        repo.path(),
        "vk/b"
    ));
    assert!(!adopt_checkout_ok(
        &task("main", wt.path(), None),
        repo.path(),
        "vk/b"
    ));
    assert!(!adopt_checkout_ok(
        &task("vk/b", wt.path(), Some("none")),
        repo.path(),
        "vk/b"
    ));
    assert!(!adopt_checkout_ok(
        &json!({"branch": "vk/b"}),
        repo.path(),
        "vk/b"
    ));
}

#[test]
fn adopted_sessions_keep_their_broker_names() {
    // Only process ids are probed (E2B); other providers' session ids are not processes.
    assert!(session_broker_script(&["s-1".into(), "abc".into()]).is_none());
    let s = session_broker_script(&["12".into(), "x;rm".into(), "345".into()]).unwrap();
    assert!(s.starts_with("for p in 12 345; do"), "{s}");
    assert!(s.contains("/proc/$p/environ"), "{s}");
    let probed = parse_session_brokers("12 ab12cd34\n345 \nbad line here\n9 ../x\n");
    assert_eq!(probed.len(), 1);
    assert_eq!(probed["12"], "ab12cd34");

    // Probed names first; else the only session of a box this host ran gets the known names.
    let known = vec!["old1".to_string(), "old2".to_string()];
    let (plan, unmapped) = plan_aliases(&["12".into()], &probed, &known);
    assert_eq!((plan, unmapped), (vec![vec!["ab12cd34".to_string()]], 0));
    let (plan, unmapped) = plan_aliases(&["s1".into()], &HashMap::new(), &known);
    assert_eq!((plan, unmapped), (vec![known.clone()], 0));
    // Several sessions without names, or a box from another host: none, and a note says so.
    let (plan, unmapped) = plan_aliases(&["s1".into(), "s2".into()], &HashMap::new(), &known);
    assert_eq!((plan, unmapped), (vec![vec![], vec![]], 2));
    let (_, unmapped) = plan_aliases(&["s1".into()], &HashMap::new(), &[]);
    assert_eq!(unmapped, 1);
    assert!(unmapped_note(1).contains("until it is restarted"));
    assert!(unmapped_note(2).starts_with("2 adopted sessions"));
}
