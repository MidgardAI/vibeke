//! PR status through a fake `gh` (05 §13). The fake is an absolute-path
//! override honoured only under `VIBEKE_TEST_HOOKS=1`; the real `gh` is never
//! run. One test function: it owns the process environment.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;
use vk_tasks::*;

/// A `gh` that logs every invocation and answers from files next to it.
fn fake_gh(dir: &Path) {
    let script = r#"#!/bin/sh
echo "$@" >> "$(dirname "$0")/calls.log"
# Never prompt: stdin must be closed and prompts disabled.
if [ "$GH_PROMPT_DISABLED" != "1" ]; then echo "prompt not disabled" >&2; exit 9; fi
if [ -t 0 ]; then echo "stdin is a tty" >&2; exit 9; fi
case "$1 $2" in
  "auth status")
    [ -f "$(dirname "$0")/unauth" ] && { echo "not logged in" >&2; exit 1; }
    exit 0 ;;
  "pr view")
    if [ -f "$(dirname "$0")/nopr" ]; then echo "no pull requests found for branch \"x\"" >&2; exit 1; fi
    cat "$(dirname "$0")/pr.json"; exit 0 ;;
esac
exit 2
"#;
    let p = dir.join("gh");
    fs::write(&p, script).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
}

fn calls(dir: &Path) -> usize {
    fs::read_to_string(dir.join("calls.log"))
        .map(|s| s.lines().filter(|l| l.starts_with("pr view")).count())
        .unwrap_or(0)
}

#[test]
fn pr_status_is_cached_authenticated_and_never_prompts() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let wt = dir.join("wt");
    fs::create_dir(&wt).unwrap();
    fake_gh(&dir);
    fs::write(
        dir.join("pr.json"),
        r#"{"number":123,"state":"OPEN","isDraft":false,"reviewDecision":"APPROVED","url":"https://example.invalid/pull/123","statusCheckRollup":[{"status":"COMPLETED","conclusion":"SUCCESS"}]}"#,
    )
    .unwrap();

    // Without the test-hook switch the override is ignored (no `gh` is run).
    // SAFETY: this test is the only one in this process.
    unsafe {
        std::env::remove_var("VIBEKE_TEST_HOOKS");
        std::env::set_var("VIBEKE_GH_BIN", dir.join("gh"));
    }
    assert_eq!(gh_binary(), Path::new("gh"));

    // Relative overrides are refused even under the switch.
    unsafe {
        std::env::set_var("VIBEKE_TEST_HOOKS", "1");
        std::env::set_var("VIBEKE_GH_BIN", "gh-relative");
    }
    assert_eq!(gh_binary(), Path::new("gh"));
    unsafe { std::env::set_var("VIBEKE_GH_BIN", dir.join("gh")) };
    assert_eq!(gh_binary(), dir.join("gh"));

    let cache = PrCache::new();
    assert_eq!(cache.peek(&wt), None, "peek never runs gh");
    assert_eq!(calls(&dir), 0);

    match cache.get(&wt, false) {
        PrLookup::Pr { pr } => {
            assert_eq!(pr.number, 123);
            assert_eq!(pr.label, "#123 ✓");
            assert_eq!(pr.review_decision.as_deref(), Some("APPROVED"));
            assert_eq!(pr.url, "https://example.invalid/pull/123");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(calls(&dir), 1);
    // Within the TTL the answer comes from the cache.
    assert!(matches!(cache.get(&wt, false), PrLookup::Pr { .. }));
    assert!(matches!(cache.peek(&wt), Some(PrLookup::Pr { .. })));
    assert_eq!(calls(&dir), 1);
    // `refresh` bypasses it.
    cache.get(&wt, true);
    assert_eq!(calls(&dir), 2);

    // A short TTL expires.
    let short = PrCache::with_ttl(Duration::from_millis(30));
    short.get(&wt, false);
    std::thread::sleep(Duration::from_millis(60));
    assert_eq!(short.peek(&wt), None);

    // No PR for the branch: cached too.
    fs::write(dir.join("nopr"), "").unwrap();
    let c2 = PrCache::new();
    assert_eq!(c2.get(&wt, false), PrLookup::NoPr);
    fs::remove_file(dir.join("nopr")).unwrap();

    // Not authenticated: no pr view is attempted, and it says why.
    fs::write(dir.join("unauth"), "").unwrap();
    let before = calls(&dir);
    match PrCache::new().get(&wt, false) {
        PrLookup::Unavailable { reason } => {
            assert!(reason.contains("not authenticated"), "{reason}")
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(calls(&dir), before);
    fs::remove_file(dir.join("unauth")).unwrap();

    // Not installed: unavailable, not an error.
    unsafe { std::env::set_var("VIBEKE_GH_BIN", dir.join("missing-gh")) };
    match PrCache::new().get(&wt, false) {
        PrLookup::Unavailable { reason } => assert!(reason.contains("not installed"), "{reason}"),
        other => panic!("{other:?}"),
    }
    unsafe {
        std::env::remove_var("VIBEKE_GH_BIN");
        std::env::remove_var("VIBEKE_TEST_HOOKS");
    }
}
