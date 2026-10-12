//! Claude's `AskUserQuestion` dialog: parsing the tabbed screen, matching it to the hook's
//! interaction and planning the answer walk.
//!
//! The screens are built from a capture of Claude Code 2.1.296 (four questions, the first
//! visible) and from the layout of its dialog components: the tab row, the question, the
//! numbered options with indented descriptions, a rule above `Chat about this` and the key
//! hints; after the last question the review with `Submit answers` and `Cancel`.

use super::screen_ask::*;
use super::*;

/// The first of four questions, as captured (a wide pane).
const Q1_WIDE: &str = "\
⏺ I have what I need. A few decisions before I write the plan.

────────────────────────────────────────────────────────────────────────────────────────────────────────
←  ☐ Scope  ☐ Publisher  ☐ Existing Qs  ☐ Who asks  ✔ Submit  →

Which hub resources should be shareable on their own through a public, no-login link in the first version?

❯ 1. Playable materials (Recommended)
     Illustrated podcast, podcast, video, document and the quiz types: anything that renders without hub chat or editing. Workbooks, chatbots, web apps and sources come later.
  2. Illustrated podcasts only
     Smallest slice: copy the existing studio share pattern for hub podcasts and generalise later.
  3. All materials + sources
     Includes uploaded source documents (signed download) and workbooks/chatbots. Bigger surface area and more anonymous-cost risk.
  4. Type something.
────────────────────────────────────────
  5. Chat about this

Enter to select · Tab/Arrow keys to navigate · Esc to cancel
";

/// The same question in a 60-column pane: the question and the descriptions wrap.
const Q1_NARROW: &str = "\
──────────────────────────────────────────────────────────
←  ☐ Scope  ☐ Publis…  ☐ Existi…  ☐ Who a…  ✔ Submit  →

Which hub resources should be shareable on their own
through a public, no-login link in the first version?

❯ 1. Playable materials (Recommended)
     Illustrated podcast, podcast, video, document and
     the quiz types: anything that renders without hub
     chat or editing.
  2. Illustrated podcasts only
     Smallest slice: copy the existing studio share
     pattern for hub podcasts and generalise later.
  3. All materials + sources
     Includes uploaded source documents (signed
     download) and workbooks/chatbots.
  4. Type something.
────────────────────────────────────────
  5. Chat about this

Enter to select · Tab/Arrow keys to navigate · Esc to
cancel
";

/// The second question, after the first was answered.
const Q2: &str = "\
────────────────────────────────────────────────────────────────────────────────
←  ☒ Scope  ☐ Publisher  ☐ Existing Qs  ☐ Who asks  ✔ Submit  →

Who should be able to publish a question so everyone listening to the podcast sees it?

❯ 1. Creator + hub owner (Recommended)
     The material's creator (hub_materials.user_id) and the hub owner, so an owner can moderate podcasts made by editors.
  2. Material creator only
     Exactly as described: only the user who generated the podcast.
  3. Creator + all editors
     Anyone with edit rights on the hub can publish questions.
  4. Type something.
────────────────────────────────────────
  5. Chat about this

Enter to select · Tab/Arrow keys to navigate · Esc to cancel
";

/// The review after the last question.
const REVIEW: &str = "\
────────────────────────────────────────────────────────────────────────────────
────────────────────────────────────────────────────────────────────────────────
←  ☒ Scope  ☒ Publisher  ☒ Existing Qs  ☒ Who asks  ✔ Submit  →

Review your answers

 ● Which hub resources should be shareable on their own through a public, no-login link in the first version?
   → Illustrated podcasts only
 ● Who should be able to publish a question so everyone listening to the podcast sees it?
   → Creator + hub owner (Recommended)
 ● What should happen to questions people have already asked, which everyone can see today?
   → Keep them public (Recommended)
 ● On a public share link, can a logged-in user who isn't a hub member ask the hosts? (Answers
   are grounded in the hub's sources, but the sources themselves stay hidden.)
   → Any logged-in user (Recommended)

Ready to submit your answers?

❯ 1. Submit answers
  2. Cancel
";

/// A single question: one tab, no arrows and no Submit tab.
const SINGLE: &str = "\
────────────────────────────────────────────────────────────────────────────────
 ☐ Database

Do you want to create the table now?

❯ 1. Yes
     Create it in this migration.
  2. No
     Leave it for later.
  3. Type something.
────────────────────────────────────────
  4. Chat about this

Enter to select · ↑/↓ to navigate · Esc to cancel
";

/// A multi-select question in a two-question dialog.
const MULTI: &str = "\
────────────────────────────────────────────────────────────────────────────────
←  ☐ Checks  ☐ Owner  ✔ Submit  →

Which checks should run before merging?

❯ 1. [ ] Unit tests
  2. [✔] Lint
  3. [ ] End-to-end tests
  4. [ ] Type something
────────────────────────────────────────
  5. Next

Enter to select · Tab/Arrow keys to navigate · Esc to cancel
";

fn lines(s: &str) -> Vec<&str> {
    s.lines().collect()
}

fn ask(s: &str) -> AskScreen {
    detect(&lines(s)).unwrap_or_else(|| panic!("no question dialog in\n{s}"))
}

/// The live interaction (i95): four questions from Claude's `PreToolUse` hook.
fn live() -> Interaction {
    let q = |question: &str, header: &str, opts: &[&str]| {
        json!({"question": question, "header": header, "multiSelect": false,
               "options": opts.iter().map(|l| json!({"label": l, "description": "…"})).collect::<Vec<_>>()})
    };
    let p = json!({"tool_name": "AskUserQuestion", "tool_use_id": "toolu_01NCA9", "tool_input": {"questions": [
        q("Which hub resources should be shareable on their own through a public, no-login link in the first version?", "Scope",
          &["Playable materials (Recommended)", "Illustrated podcasts only", "All materials + sources"]),
        q("Who should be able to publish a question so everyone listening to the podcast sees it?", "Publisher",
          &["Creator + hub owner (Recommended)", "Material creator only", "Creator + all editors"]),
        q("What should happen to questions people have already asked, which everyone can see today?", "Existing Qs",
          &["Keep them public (Recommended)", "Make them private"]),
        q("On a public share link, can a logged-in user who isn't a hub member ask the hosts? (Answers are grounded in the hub's sources, but the sources themselves stay hidden.)", "Who asks",
          &["Any logged-in user (Recommended)", "Hub members only"]),
    ]}});
    let mut it = harness::interaction_from_hook(Harness::Claude, "PreToolUse", &p).unwrap();
    it.answer_channel = AnswerChannel::Keystrokes;
    it
}

fn answer_all(it: &Interaction, picks: &[&str]) -> Answer {
    Answer {
        decision: None,
        choices: it
            .questions
            .iter()
            .zip(picks)
            .map(|(q, p)| (q.id.clone(), vec![p.to_string()]))
            .collect(),
        text: None,
    }
}

/// `a` with the pointer on row `i` (the walk's view after moving).
fn pointed(mut a: AskScreen, i: usize) -> AskScreen {
    a.pointer = Some(i);
    a
}

#[test]
fn parses_the_tabbed_question() {
    let a = ask(Q1_WIDE);
    assert_eq!(a.view, AskView::Question);
    let headers: Vec<&str> = a.tabs.iter().map(|t| t.header.as_str()).collect();
    assert_eq!(headers, ["Scope", "Publisher", "Existing Qs", "Who asks"]);
    assert!(a.tabs.iter().all(|t| !t.answered));
    assert!(a.submit_tab);
    assert_eq!(
        a.title,
        "Which hub resources should be shareable on their own through a public, no-login link in the first version?"
    );
    let labels: Vec<&str> = a.rows.iter().map(|r| r.label.as_str()).collect();
    assert_eq!(
        labels,
        [
            "Playable materials (Recommended)",
            "Illustrated podcasts only",
            "All materials + sources",
            "Type something.",
            "Chat about this"
        ]
    );
    assert_eq!(a.pointer, Some(0));
    assert!(
        a.rows[0]
            .description
            .as_deref()
            .unwrap()
            .starts_with("Illustrated podcast, podcast")
    );
    assert_eq!(a.options().count(), 3);
    assert!(!a.multi());
    // As a screen dialog: a question whose options are the three answers.
    let d = screen::evaluate(Harness::Claude, Q1_WIDE).dialog.unwrap();
    assert_eq!(d.kind, InteractionKind::Question);
    assert_eq!(d.options.len(), 3);
    assert_eq!(d.pointer, Some(1));
    assert!(d.picker.is_none(), "not the unknown-dialog picker");
}

#[test]
fn a_narrow_pane_joins_the_wrapped_question() {
    let wide = ask(Q1_WIDE);
    let narrow = ask(Q1_NARROW);
    assert_eq!(narrow.title, wide.title);
    assert_eq!(narrow.rows.len(), 5);
    assert_eq!(
        narrow.rows[1].description.as_deref(),
        Some(
            "Smallest slice: copy the existing studio share pattern for hub podcasts and generalise later."
        )
    );
    assert!(matches(&narrow, &live()));
}

#[test]
fn parses_the_second_question_and_the_review() {
    let a = ask(Q2);
    assert!(a.tabs[0].answered && !a.tabs[1].answered);
    assert_eq!(question_index(&a, &live()), Some(1));
    let r = ask(REVIEW);
    assert_eq!(r.view, AskView::Review);
    assert_eq!(r.title, REVIEW_TITLE);
    assert!(r.tabs.iter().all(|t| t.answered));
    let labels: Vec<&str> = r.rows.iter().map(|x| x.label.as_str()).collect();
    assert_eq!(labels, ["Submit answers", "Cancel"]);
    assert!(r.body.contains("→ Illustrated podcasts only"));
    assert!(r.body.contains("Answers are grounded"));
}

#[test]
fn a_single_question_and_a_multi_select_question() {
    let a = ask(SINGLE);
    assert_eq!(a.tabs.len(), 1);
    assert!(!a.submit_tab);
    // Not read as a permission prompt despite "Do you want to create …?" and a Yes option.
    let d = screen::evaluate(Harness::Claude, SINGLE).dialog.unwrap();
    assert_eq!(d.kind, InteractionKind::Question);
    assert!(d.ask.is_some());
    let m = ask(MULTI);
    assert!(m.multi());
    assert_eq!(m.rows[1].checked, Some(true));
    assert_eq!(m.rows[1].label, "Lint");
}

#[test]
fn other_screens_are_not_the_question_dialog() {
    // Claude's todo list uses the same boxes, without the dialog around it.
    let todo = "⏺ Update Todos\n  ⎿  ☐ Fix the tests\n     ☒ Write the plan\n\n> \n";
    assert!(detect(&lines(todo)).is_none());
    // A footer without the tab row.
    let no_tabs = "Pick one\n\n❯ 1. A\n  2. B\n\nEnter to select · ↑/↓ to navigate · Esc to cancel";
    assert!(detect(&lines(no_tabs)).is_none());
    // Unknown text between the options and the footer: fail closed.
    let extra = Q2.replace(
        "  5. Chat about this",
        "  5. Chat about this\nsomething else",
    );
    assert!(detect(&lines(&extra)).is_none());
}

/// Live bug: answering i95 failed with `dialog_changed` although its first question was on
/// screen (the screen read it as an unknown dialog). Now every one of its questions, and its
/// review, match; another interaction's dialog does not.
#[test]
fn the_dialog_matches_any_of_its_questions_and_the_review() {
    let it = live();
    for s in [Q1_WIDE, Q1_NARROW, Q2, REVIEW] {
        let d = screen::evaluate(Harness::Claude, s).dialog.unwrap();
        assert!(dialog_matches(&d, &it), "{s}");
    }
    // Another call's question with the same headers.
    let mut other = live();
    other.questions[1].prompt = "Who reviews the questions?".into();
    let d = screen::evaluate(Harness::Claude, Q2).dialog.unwrap();
    assert!(!dialog_matches(&d, &other));
    // A dialog with other tabs (a different question count) is not this interaction.
    let mut three = live();
    three.questions.pop();
    let d = screen::evaluate(Harness::Claude, Q1_WIDE).dialog.unwrap();
    assert!(!dialog_matches(&d, &three));
    // Renamed tab headers are another dialog.
    let mut renamed = live();
    renamed.questions[0].header = Some("Sharing".into());
    assert!(!dialog_matches(&d, &renamed));
    // The same question with other options is a replacement dialog.
    let mut reworded = live();
    reworded.questions[0].options[1].label = "Podcasts only".into();
    assert!(!dialog_matches(&d, &reworded));
    let mut fewer = live();
    fewer.questions[0].options.pop();
    assert!(!dialog_matches(&d, &fewer));
}

#[test]
fn walk_plan_answers_each_question_then_submits() {
    let it = live();
    let ans = answer_all(
        &it,
        &[
            "Illustrated podcasts only",
            "Creator + hub owner (Recommended)",
            "Keep them public (Recommended)",
            "Any logged-in user (Recommended)",
        ],
    );
    let want = wanted(&it, &ans);
    let mut done = vec![false; 4];
    // Question 1: move down to the second option, then select it.
    let q1 = ask(Q1_WIDE);
    assert_eq!(
        next_step(&q1, &it, &want, &done),
        AskStep::Key("down".into())
    );
    assert_eq!(
        next_step(&pointed(q1, 1), &it, &want, &done),
        AskStep::Select {
            key: "enter".into(),
            question: Some(0)
        }
    );
    done[0] = true;
    // Question 2: the pointed option is the answer.
    let q2 = ask(Q2);
    assert_eq!(
        next_step(&q2, &it, &want, &done),
        AskStep::Select {
            key: "enter".into(),
            question: Some(1)
        }
    );
    // The review before every question was entered: go back to the first open one.
    let review = ask(REVIEW);
    assert_eq!(
        next_step(&review, &it, &want, &done),
        AskStep::Key("left".into())
    );
    // Every question entered: Submit answers.
    let done = vec![true; 4];
    assert_eq!(
        next_step(&review, &it, &want, &done),
        AskStep::Select {
            key: "enter".into(),
            question: None
        }
    );
    assert!(needs_submit(&review, &it));
    // The pointer on Cancel moves up first.
    assert_eq!(
        next_step(&pointed(review.clone(), 1), &it, &want, &done),
        AskStep::Key("up".into())
    );
    // A review that lists other answers (the user changed one in the pane) is not submitted.
    let other = answer_all(
        &it,
        &[
            "All materials + sources",
            "Creator + hub owner (Recommended)",
            "Keep them public (Recommended)",
            "Any logged-in user (Recommended)",
        ],
    );
    assert!(matches!(
        next_step(&review, &it, &wanted(&it, &other), &done),
        AskStep::Fail(r) if r.contains("All materials + sources")
    ));
}

#[test]
fn walk_plan_switches_tabs_to_an_unanswered_question() {
    let it = live();
    let ans = answer_all(
        &it,
        &[
            "Illustrated podcasts only",
            "Material creator only",
            "Make them private",
            "Hub members only",
        ],
    );
    let want = wanted(&it, &ans);
    // Question 2 shows, but question 1 is entered already: go on to question 3.
    let done = vec![true, true, false, false];
    let q2 = ask(Q2);
    assert_eq!(
        next_step(&q2, &it, &want, &done),
        AskStep::Key("right".into())
    );
    // On the free-text field the arrows move its cursor: step off it first.
    let on_text = pointed(ask(Q2), 3);
    assert_eq!(
        next_step(&on_text, &it, &want, &done),
        AskStep::Key("up".into())
    );
}

#[test]
fn walk_plan_refuses_what_it_cannot_type() {
    let it = live();
    // Free text: no row shows it.
    let ans = answer_all(&it, &["Only podcasts, but with transcripts", "x", "x", "x"]);
    assert!(matches!(
        next_step(&ask(Q1_WIDE), &it, &wanted(&it, &ans), &[false; 4]),
        AskStep::Fail(r) if r.starts_with("answer_in_pane")
    ));
    // A missing choice.
    let partial = Answer {
        choices: vec![(
            it.questions[1].id.clone(),
            vec!["Material creator only".into()],
        )],
        ..Default::default()
    };
    assert!(matches!(
        next_step(&ask(Q1_WIDE), &it, &wanted(&it, &partial), &[false; 4]),
        AskStep::Fail(r) if r.starts_with("answer_in_pane")
    ));
    // A multi-select question.
    let p = json!({"tool_name": "AskUserQuestion", "tool_input": {"questions": [
        {"question": "Which checks should run before merging?", "header": "Checks", "multiSelect": true,
         "options": [{"label": "Unit tests"}, {"label": "Lint"}]},
        {"question": "Who owns the release?", "header": "Owner", "multiSelect": false,
         "options": [{"label": "Me"}, {"label": "The team"}]}]}});
    let multi = harness::interaction_from_hook(Harness::Claude, "PreToolUse", &p).unwrap();
    let ans = answer_all(&multi, &["Lint", "Me"]);
    assert!(matches!(
        next_step(&ask(MULTI), &multi, &wanted(&multi, &ans), &[false; 2]),
        AskStep::Fail(r) if r.contains("multi-select")
    ));
}

/// A screen-sourced interaction (no hooks) describes one view; its answer selects that view's
/// option and is done when the dialog moves on.
#[test]
fn a_screen_sourced_view_is_answered_on_its_own() {
    let d = screen::evaluate(Harness::Claude, Q2).dialog.unwrap();
    let it = Interaction {
        kind: InteractionKind::Question,
        questions: d.options_as_question(),
        native_ref: Some(format!("screen:{}", d.fingerprint)),
        source: StateSource::Screen,
        ..harness_tests_blank()
    };
    assert!(dialog_matches(&d, &it));
    let a = d.ask.clone().unwrap();
    assert!(matches(&a, &it));
    let ans = Answer {
        choices: vec![("q0".into(), vec!["2".into()])],
        ..Default::default()
    };
    let want = wanted(&it, &ans);
    assert_eq!(want, [Some("Material creator only".to_string())]);
    assert_eq!(
        next_step(&a, &it, &want, &[false]),
        AskStep::Key("down".into())
    );
    assert!(!needs_submit(&a, &it));
}

#[test]
fn tab_rows() {
    let (tabs, submit) = tab_row("←  ☒ Scope  ☐ Publisher  ✔ Submit  →").unwrap();
    assert!(submit);
    assert_eq!(tabs.len(), 2);
    assert!(tabs[0].answered);
    // Too narrow for every header: only the current one is shown.
    let (tabs, _) = tab_row("←  ☐ Sco…  ☐   ☐   ✔ Submit  →").unwrap();
    assert_eq!(tabs.len(), 3);
    assert_eq!(tabs[1].header, "");
    assert!(tab_row(" ☐ Database ").is_some());
    // Several tabs without Submit, or Submit in the middle, are not a tab row.
    assert!(tab_row("☐ Fix tests ☐ Ship").is_none());
    assert!(tab_row("←  ☐ A  ✔ Submit  ☐ B  →").is_none());
    assert!(tab_row("Plain text").is_none());
}
