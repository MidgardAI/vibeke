//! PR evidence lookup through a fake `gh` (15 §6.4, lane 3F). The fake is an absolute-path
//! override honoured only under `VIBEKE_TEST_HOOKS=1`; the real `gh` is never run. One test
//! function: it owns the process environment.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use vk_tasks::*;

fn fake_gh(dir: &Path) {
    let script = r#"#!/bin/sh
echo "$@" >> "$(dirname "$0")/calls.log"
if [ "$GH_PROMPT_DISABLED" != "1" ]; then echo "prompt not disabled" >&2; exit 9; fi
case "$1 $2" in
  "auth status")
    [ -f "$(dirname "$0")/unauth" ] && { echo "not logged in" >&2; exit 1; }
    exit 0 ;;
  "pr view")
    [ -f "$(dirname "$0")/offline" ] && { echo "error connecting to api.github.com" >&2; exit 1; }
    if [ -f "$(dirname "$0")/nopr" ]; then echo "no pull requests found for branch \"x\"" >&2; exit 1; fi
    cat "$(dirname "$0")/pr.json"; exit 0 ;;
esac
exit 2
"#;
    let p = dir.join("gh");
    fs::write(&p, script).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
}

fn view_calls(dir: &Path) -> Vec<String> {
    fs::read_to_string(dir.join("calls.log"))
        .map(|s| {
            s.lines()
                .filter(|l| l.starts_with("pr view"))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn pr_evidence_json_is_fetched_with_the_identity_fields_and_never_prompts() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let wt = dir.join("wt");
    fs::create_dir(&wt).unwrap();
    fake_gh(&dir);
    let json = r#"{"number":7,"url":"https://github.com/acme/app/pull/7","state":"OPEN","isDraft":false,"baseRefName":"main","headRefName":"feat","headRefOid":"a1b2c3d4e5f60718293a4b5c6d7e8f9012345678","reviewDecision":"","statusCheckRollup":[]}"#;
    fs::write(dir.join("pr.json"), json).unwrap();
    // SAFETY: this test is the only one in this process.
    unsafe {
        std::env::set_var("VIBEKE_TEST_HOOKS", "1");
        std::env::set_var("VIBEKE_GH_BIN", dir.join("gh"));
    }

    // The checked-out branch's PR: the identity fields are requested.
    assert_eq!(
        fetch_pr_evidence_json(&wt, None),
        PrJson::Found(json.to_string())
    );
    let calls = view_calls(&dir);
    assert_eq!(calls.len(), 1);
    for f in [
        "number",
        "url",
        "state",
        "isDraft",
        "baseRefName",
        "headRefName",
        "headRefOid",
        "reviewDecision",
        "statusCheckRollup",
    ] {
        assert!(calls[0].contains(f), "{f} missing from {}", calls[0]);
    }
    // An explicit reference is passed through as one argument.
    fetch_pr_evidence_json(&wt, Some("7"));
    assert!(view_calls(&dir)[1].starts_with("pr view 7 --json"));

    // Option-like and malformed references never reach gh.
    let before = view_calls(&dir).len();
    for bad in ["--exec=evil", "-R", "a b", "", "x\ny"] {
        assert!(
            matches!(
                fetch_pr_evidence_json(&wt, Some(bad)),
                PrJson::Unavailable(_)
            ),
            "{bad:?}"
        );
    }
    assert_eq!(view_calls(&dir).len(), before);
    assert!(valid_pr_ref("feature/x") && valid_pr_ref("https://github.com/a/b/pull/1"));

    // No PR for the branch.
    fs::write(dir.join("nopr"), "").unwrap();
    assert_eq!(fetch_pr_evidence_json(&wt, None), PrJson::NoPr);
    fs::remove_file(dir.join("nopr")).unwrap();

    // Offline: unavailable with gh's first line; never a pass or failure.
    fs::write(dir.join("offline"), "").unwrap();
    match fetch_pr_evidence_json(&wt, None) {
        PrJson::Unavailable(r) => assert!(r.contains("error connecting"), "{r}"),
        other => panic!("{other:?}"),
    }
    fs::remove_file(dir.join("offline")).unwrap();

    // Not authenticated: no `pr view` is attempted.
    fs::write(dir.join("unauth"), "").unwrap();
    let before = view_calls(&dir).len();
    match fetch_pr_evidence_json(&wt, None) {
        PrJson::Unavailable(r) => assert!(r.contains("not authenticated"), "{r}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(view_calls(&dir).len(), before);
    fs::remove_file(dir.join("unauth")).unwrap();

    // Not installed.
    unsafe { std::env::set_var("VIBEKE_GH_BIN", dir.join("missing-gh")) };
    match fetch_pr_evidence_json(&wt, None) {
        PrJson::Unavailable(r) => assert!(r.contains("not installed"), "{r}"),
        other => panic!("{other:?}"),
    }
    unsafe {
        std::env::remove_var("VIBEKE_GH_BIN");
        std::env::remove_var("VIBEKE_TEST_HOOKS");
    }
}
