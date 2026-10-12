use super::*;

const PROMPT: &str = "@helper What is 2+2? Answer with one word.";

fn draft(t: &str) -> InputBox {
    InputBox::Draft(t.to_string())
}

#[test]
fn the_restored_prompt_matches_whitespace_aside() {
    assert_eq!(seen(&draft(PROMPT), PROMPT), Seen::Restored);
    // A soft wrap may split the line anywhere, even inside a word.
    assert_eq!(
        seen(
            &draft("@helper What is 2+2? Answer with one wo\nrd."),
            PROMPT
        ),
        Seen::Restored
    );
    assert_eq!(
        seen(
            &draft("  @helper  What is 2+2?\n  Answer with one word.  "),
            PROMPT
        ),
        Seen::Restored
    );
}

#[test]
fn other_text_is_the_persons() {
    // The person started editing the restored prompt.
    assert_eq!(seen(&draft(&format!("{PROMPT} Also")), PROMPT), Seen::Other);
    assert_eq!(seen(&draft("@helper What is 2+2?"), PROMPT), Seen::Other);
    assert_eq!(seen(&draft("something else"), PROMPT), Seen::Other);
    // A pasted-text chip is never the prompt.
    assert_eq!(
        seen(&draft("[Pasted text #1 +20 lines]"), PROMPT),
        Seen::Other
    );
}

#[test]
fn an_empty_or_unreadable_box_keeps_the_watcher_waiting() {
    assert_eq!(seen(&InputBox::Empty, PROMPT), Seen::Wait);
    assert_eq!(seen(&InputBox::Unknown, PROMPT), Seen::Wait);
}

#[test]
fn clearing_kills_lines_until_the_box_is_empty() {
    assert_eq!(step(&draft(PROMPT), PROMPT, None), Step::Kill);
    // A multi-line prompt: the last line went, the start is left.
    let multi = "@helper first line\nsecond line";
    assert_eq!(
        step(&draft("@helper first line"), multi, Some(&compact(multi))),
        Step::Kill
    );
    assert_eq!(step(&InputBox::Empty, multi, None), Step::Done);
    assert_eq!(step(&draft("   "), multi, None), Step::Done);
}

#[test]
fn a_key_that_changed_nothing_is_followed_by_backspace() {
    let multi = "@helper first line\nsecond line";
    let left = "@helper first line";
    assert_eq!(
        step(&draft(left), multi, Some(&compact(left))),
        Step::Backspace
    );
}

#[test]
fn clearing_stops_when_the_box_holds_other_text() {
    // The person typed into the box while it was being cleared.
    assert_eq!(
        step(
            &draft("@helper first linex"),
            "@helper first line\nmore",
            None
        ),
        Step::Stop
    );
    assert_eq!(step(&draft("hello"), PROMPT, None), Step::Stop);
    assert_eq!(step(&InputBox::Unknown, PROMPT, None), Step::Stop);
}

#[test]
fn only_claude_and_codex_are_cleared() {
    assert!(applies(Harness::from_id("claude").unwrap()));
    assert!(applies(Harness::from_id("codex").unwrap()));
}
