use super::*;

fn none(_: &str) -> bool {
    false
}

#[test]
fn names_follow_the_rule() {
    assert!(valid("reviewer"));
    assert!(valid("test-runner_2"));
    assert!(!valid("2fast"));
    assert!(!valid("Reviewer"));
    assert!(!valid("current"));
    assert!(!valid(&"a".repeat(33)));
}

#[test]
fn suggestions_come_from_the_label() {
    assert_eq!(
        suggest("Fix the login redirect bug", "claude", none),
        "fix-login-redirect"
    );
    assert_eq!(
        suggest("Review retry logic in client.rs", "claude", none),
        "review-retry-logic"
    );
    assert_eq!(suggest("", "claude", none), "claude");
    assert_eq!(suggest("zsh · ~/code/app", "app", none), "zsh-code-app");
    assert_eq!(suggest("2024 — 🚀", "codex", none), "codex");
    assert_eq!(suggest("current", "codex", none), "codex");
}

#[test]
fn suggestions_avoid_taken_names() {
    let taken = |n: &str| n == "claude" || n == "claude-2";
    assert_eq!(suggest("claude", "claude", taken), "claude-3");
    let long = "abcdefghijklmnopqrstuvwxyzabcdef";
    let taken = |n: &str| n == long;
    let s = suggest(long, "x", taken);
    assert!(s.len() <= MAX_LEN && s.ends_with("-2") && valid(&s), "{s}");
}

#[test]
fn leading_and_embedded_mentions() {
    let m = parse("@reviewer @tester, check the retry logic");
    assert_eq!(m.leading, ["reviewer", "tester"]);
    assert!(m.embedded.is_empty());

    let m = parse("@a, @b fix it");
    assert_eq!(m.leading, ["a", "b"]);

    let m = parse("Ask @reviewer (and @tester) to look. Thanks @reviewer.");
    assert!(m.leading.is_empty());
    assert_eq!(m.embedded, ["reviewer", "tester"]);
}

#[test]
fn paths_emails_and_escapes_stay_literal() {
    let m = parse(
        "see @src/main.rs and @README.md, mail me@example.com, \\@reviewer, @types/node, @@x @Foo @ x",
    );
    assert_eq!(m, Mentions::default());
    let m = parse("look at @notes.txt and ask @reviewer.");
    assert_eq!(m.embedded, ["reviewer"]);
}
