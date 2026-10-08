//! Picker detection and key planning against recorded screens (`tests/picker-screens`).

use super::Harness;
use super::screen;
use super::screen_picker::*;
use std::path::Path;
use vk_proto::model::*;

fn rec(rel: &str) -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/picker-screens")
        .join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

fn picker(h: Harness, rel: &str) -> Option<Picker> {
    screen::evaluate(h, &rec(rel)).dialog.and_then(|d| d.picker)
}

fn labels(p: &Picker) -> Vec<&str> {
    p.rows.iter().map(|r| r.label.as_str()).collect()
}

/// The pointed row's id without the stable-facts hash a scrolling list adds (`opus-5-5~1a2b3c`).
fn pointed(p: &Picker) -> &str {
    base(&p.rows[p.pointer.unwrap()].id)
}

fn base(id: &str) -> &str {
    id.split('~').next().unwrap()
}

/// The option id of the row labelled `label`.
fn id_of(p: &Picker, label: &str) -> String {
    p.rows
        .iter()
        .find(|r| r.label == label)
        .unwrap_or_else(|| panic!("no row {label}"))
        .id
        .clone()
}

// ---- Claude ---------------------------------------------------------------------------------------

#[test]
fn claude_model_picker() {
    let p = picker(Harness::Claude, "claude/2.1.295/model.txt").expect("model picker");
    assert_eq!(p.name, "model");
    assert_eq!(p.title, "Select model");
    assert_eq!(
        labels(&p),
        [
            "Default (recommended)",
            "Opus 5.5",
            "Fable 5.1",
            "Sonnet 5.5",
            "Haiku 5.5",
            "Haiku 4.5",
            "Sonnet 5"
        ]
    );
    assert_eq!(pointed(&p), "opus-5-5");
    assert_eq!(
        p.rows[1].description.as_deref(),
        Some("Current. For complex work and everyday tasks")
    );
    assert!(p.scrolls, "`… +6 models` and `↓` mark a partial list");
    assert!(!p.multi);
    assert_eq!(p.keys.confirm.as_deref(), Some("enter"));
    assert_eq!(p.keys.session.as_deref(), Some("s"));
    assert_eq!(p.keys.cancel.as_deref(), Some("escape"));
    let a = p.adjust.as_ref().expect("effort adjuster");
    assert_eq!(a.current.as_deref(), Some("medium"));
    assert_eq!(a.values, CLAUDE_EFFORTS);
    // The interaction shape: options with the pointed row selected, picker info.
    let qs = screen::picker_questions(&p);
    assert_eq!(qs[0].id, "q0");
    assert!(qs[0].options[1].selected && !qs[0].options[0].selected);
    let info = screen::picker_info(&p);
    assert_eq!(info.cancel_key.as_deref(), Some("Escape"));
    assert!(info.up_down);
    assert_eq!(info.source, "screen");
}

#[test]
fn claude_model_signature_survives_pointer_moves_and_scrolling() {
    let a = picker(Harness::Claude, "claude/2.1.295/model.txt").unwrap();
    let b = picker(Harness::Claude, "claude/2.1.295/model-scrolled.txt").unwrap();
    let c = picker(Harness::Claude, "claude/2.1.295/model-scrolled-up.txt").unwrap();
    assert_eq!(a.signature, b.signature);
    assert_eq!(a.signature, c.signature);
    assert_eq!(pointed(&b), "opus-5");
    assert_eq!(b.rows.first().unwrap().label, "Opus 5.5");
    assert_eq!(pointed(&c), "sonnet-5");
    // Each model shows its own effort.
    assert_eq!(b.adjust.unwrap().current.as_deref(), Some("xhigh"));
}

#[test]
fn claude_model_picker_in_a_small_pane() {
    // As a Vibeke pane renders it at 80x24: two rows and `… +11 models`.
    let p = picker(Harness::Claude, "claude/2.1.295/model-80x24.txt").expect("model picker");
    assert_eq!(p.name, "model");
    assert_eq!(p.title, "Select model");
    assert_eq!(labels(&p), ["Default (recommended)", "Opus 5.5"]);
    assert_eq!(pointed(&p), "opus-5-5");
    assert!(p.scrolls);
    assert_eq!(p.adjust.unwrap().current.as_deref(), Some("medium"));
}

#[test]
fn claude_effort_slider() {
    let p = picker(Harness::Claude, "claude/2.1.295/effort.txt").expect("effort picker");
    assert_eq!(p.name, "effort");
    assert_eq!(p.title, "Effort");
    let a = p.adjust.as_ref().unwrap();
    assert_eq!(a.values, ["low", "medium", "high", "xhigh", "max"]);
    assert_eq!(a.current.as_deref(), Some("medium"));
    assert_eq!(pointed(&p), "medium");
    assert_eq!(p.keys.left.as_deref(), Some("left"));
    assert_eq!(p.keys.session.as_deref(), Some("s"));
    assert!(!screen::picker_info(&p).up_down);
    for (f, want) in [("effort-high.txt", "high"), ("effort-max.txt", "max")] {
        let q = picker(Harness::Claude, &format!("claude/2.1.295/{f}")).unwrap();
        assert_eq!(q.adjust.unwrap().current.as_deref(), Some(want), "{f}");
        assert_eq!(q.signature, p.signature, "{f}");
    }
}

#[test]
fn claude_resume_list() {
    let p = picker(Harness::Claude, "claude/2.1.295/resume.txt").expect("resume picker");
    assert_eq!(p.name, "resume");
    assert!(p.title.starts_with("Resume session"));
    assert_eq!(p.rows.len(), 6, "the project header is not a session");
    assert_eq!(pointed(&p), "fix-flaky-upload-test");
    assert_eq!(
        p.rows[0].description.as_deref(),
        Some("1 second ago · main · 2.8MB · example/app#4")
    );
    assert_eq!(p.rows[5].label, "Rename the settings page");
    assert_eq!(p.keys.confirm.as_deref(), Some("enter"));
    assert_eq!(p.keys.cancel.as_deref(), Some("escape"));
    assert!(p.keys.session.is_none());
}

#[test]
fn claude_unreadable_modals_fall_back_to_unknown() {
    // The permissions editor (tabs, a list without a pointer while the search box has focus)
    // and settings (> 9 rows, changes rather than picks) are not menus Vibeke can drive.
    for f in ["permissions.txt", "config.txt"] {
        let p = picker(Harness::Claude, &format!("claude/2.1.295/{f}")).expect(f);
        assert!(p.is_unknown(), "{f}: {}", p.name);
        assert_eq!(p.title, UNKNOWN_TITLE);
        assert!(p.rows.is_empty());
        assert_eq!(p.keys.cancel.as_deref(), Some("escape"));
    }
}

#[test]
fn claude_input_screens_are_not_pickers() {
    for f in ["start.txt", "after-esc.txt", "slash-autocomplete.txt"] {
        let m = screen::evaluate(Harness::Claude, &rec(&format!("claude/2.1.295/{f}")));
        assert!(m.dialog.is_none(), "{f}: {:?}", m.dialog);
    }
}

// ---- Codex ----------------------------------------------------------------------------------------

#[test]
fn codex_model_picker() {
    let p = picker(Harness::Codex, "codex/0.161.0/model.txt").expect("model picker");
    assert_eq!(p.name, "model");
    assert_eq!(p.title, "Select Model and Effort");
    assert_eq!(p.rows.len(), 7);
    assert_eq!(p.rows[1].label, "GPT-6-Astra (current)");
    assert_eq!(
        p.rows[1].description.as_deref(),
        Some("Frontier intelligence for the most demanding work.")
    );
    assert_eq!(pointed(&p), "gpt-6-astra-current");
    assert_eq!(p.keys.confirm.as_deref(), Some("enter"));
    assert_eq!(p.keys.cancel.as_deref(), Some("escape"));
    assert_eq!(p.keys.up.as_deref(), Some("up"));
    assert!(!p.scrolls);
    let q = picker(Harness::Codex, "codex/0.161.0/model-down.txt").unwrap();
    assert_eq!(q.signature, p.signature);
    assert_eq!(pointed(&q), "gpt-6-sol");
}

#[test]
fn codex_permissions_picker_with_wrapped_descriptions() {
    let p = picker(Harness::Codex, "codex/0.161.0/permissions.txt").expect("permissions");
    assert_eq!(p.name, "permissions");
    assert_eq!(
        labels(&p),
        [
            "Ask for approval (current)",
            "Approve for me",
            "Full Access",
            "Read Only"
        ]
    );
    assert_eq!(
        p.rows[0].description.as_deref(),
        Some(
            "Read and edit workspace files and run commands, with approval required for internet access or edits outside the workspace"
        )
    );
    assert_eq!(p.pointer, Some(0));
}

#[test]
fn codex_startup_menu_is_a_generic_menu() {
    let p = picker(Harness::Codex, "codex/0.161.0/start.txt").expect("hooks menu");
    assert_eq!(p.name, "menu");
    assert_eq!(p.title, "Hooks need review");
    assert_eq!(
        p.body.as_deref(),
        Some(
            "12 hooks are new or changed. Hooks can run outside the sandbox after you trust them."
        )
    );
    assert_eq!(p.rows.len(), 3);
    assert_eq!(pointed(&p), "review-hooks");
    assert_eq!(p.keys.cancel.as_deref(), Some("escape"));
}

#[test]
fn codex_resume_fails_closed_to_unknown() {
    let p = picker(Harness::Codex, "codex/0.161.0/resume.txt").expect("dialog");
    assert!(
        p.is_unknown(),
        "{} rows without an understood indicator",
        p.rows.len()
    );
    assert_eq!(p.keys.cancel.as_deref(), Some("escape"));
}

#[test]
fn codex_input_screens_are_not_pickers() {
    for f in ["ready.txt", "slash.txt"] {
        let m = screen::evaluate(Harness::Codex, &rec(&format!("codex/0.161.0/{f}")));
        assert!(m.dialog.is_none(), "{f}: {:?}", m.dialog);
    }
}

// ---- omp ------------------------------------------------------------------------------------------

#[test]
fn omp_model_selector_is_unknown() {
    let p = picker(Harness::Omp, "omp/17.2.12/model.txt").expect("dialog");
    assert!(p.is_unknown());
    assert_eq!(p.keys.cancel.as_deref(), Some("escape"));
}

// ---- generic menu: negatives and multi-select ---------------------------------------------------

#[test]
fn generic_menu_fails_closed() {
    // No footer.
    assert!(generic_menu(&["Pick one", "", "❯ 1. Alpha", "  2. Beta"]).is_none());
    // Footer without a confirm or cancel key.
    assert!(
        generic_menu(&[
            "Pick one",
            "",
            "❯ 1. Alpha",
            "  2. Beta",
            "",
            "Tab to switch · ? for help"
        ])
        .is_none()
    );
    // Two pointers.
    assert!(
        generic_menu(&[
            "Pick one",
            "",
            "❯ 1. Alpha",
            "❯ 2. Beta",
            "",
            "Enter to confirm · Esc to cancel"
        ])
        .is_none()
    );
    // Ten rows with no scroll indicator.
    let mut l = vec!["Pick one".to_string(), String::new()];
    for i in 0..10 {
        l.push(format!(
            "{} {}. Option {i}",
            if i == 0 { "❯" } else { " " },
            i + 1
        ));
    }
    l.push(String::new());
    l.push("Enter to confirm · Esc to cancel".into());
    let refs: Vec<&str> = l.iter().map(String::as_str).collect();
    assert!(generic_menu(&refs).is_none());
    // A working agent never shows a picker.
    let busy = "Pick one\n\n❯ 1. Alpha\n  2. Beta\n\n✻ Thinking… (esc to interrupt)";
    assert!(screen::evaluate(Harness::Claude, busy).dialog.is_none());
}

#[test]
fn generic_menu_pointer_position_and_confirmation_name() {
    let screen = [
        "Switch model?",
        "This conversation will continue on the new model.",
        "",
        "  1. Yes, switch",
        "❯ 2. No, keep the current model",
        "",
        "Enter to confirm · Esc to cancel",
    ];
    let p = generic_menu(&screen).unwrap();
    assert_eq!(p.name, "confirm");
    assert_eq!(p.pointer, Some(1));
    assert_eq!(p.rows[0].id, "yes-switch");
}

/// Constructed (not recorded): checkbox rows as a multi-select list draws them.
const MULTI: &[&str] = &[
    "Which checks should run?",
    "",
    "❯ [x] Lint",
    "  [ ] Unit tests",
    "  [x] Type check",
    "",
    "↑/↓ to move · Space to toggle · Enter to submit · Esc to cancel",
];

#[test]
fn multi_select_states() {
    let p = generic_menu(MULTI).unwrap();
    assert!(p.multi);
    let checked: Vec<bool> = p.rows.iter().map(|r| r.checked.unwrap()).collect();
    assert_eq!(checked, [true, false, true]);
    assert_eq!(p.keys.toggle.as_deref(), Some("space"));
    assert_eq!(p.keys.confirm.as_deref(), Some("enter"));
    let qs = screen::picker_questions(&p);
    assert!(qs[0].multi);
    let sel: Vec<bool> = qs[0].options.iter().map(|o| o.selected).collect();
    assert_eq!(
        sel,
        [true, false, true],
        "selected = checked in a multi-select"
    );
}

// ---- key planning ---------------------------------------------------------------------------------

fn step(p: &Picker, g: &Goal) -> Step {
    let order: Vec<String> = p.rows.iter().map(|r| r.id.clone()).collect();
    next_step(p, g, &order)
}

#[test]
fn planning_single_select_walks_then_commits_session_only() {
    let p = picker(Harness::Claude, "claude/2.1.295/model.txt").unwrap();
    let g = |t: &str| Goal {
        target: Some(t.into()),
        ..Default::default()
    };
    let id = |l: &str| id_of(&p, l);
    assert_eq!(step(&p, &g(&id("Sonnet 5.5"))), Step::Key("down".into()));
    assert_eq!(
        step(&p, &g(&id("Default (recommended)"))),
        Step::Key("up".into())
    );
    // On the target: commit with the session-only key, never Enter (which persists a default).
    assert_eq!(step(&p, &g(&id("Opus 5.5"))), Step::Commit("s".into()));
    let persist = Goal {
        persist: true,
        ..g(&id("Opus 5.5"))
    };
    assert_eq!(step(&p, &persist), Step::Commit("enter".into()));
    // Never a digit.
    for t in ["Fable 5.1", "Haiku 4.5", "Sonnet 5"] {
        assert!(matches!(step(&p, &g(&id(t))), Step::Key(k) if k == "down"));
    }
}

#[test]
fn planning_uses_the_recorded_order_for_rows_scrolled_out_of_view() {
    let p = picker(Harness::Claude, "claude/2.1.295/model-scrolled.txt").unwrap();
    let top = picker(Harness::Claude, "claude/2.1.295/model.txt").unwrap();
    // The order recorded when the interaction opened (the top of the list), then the rows
    // scrolled into view since: one id per row in every window.
    let mut order: Vec<String> = top.rows.iter().map(|r| r.id.clone()).collect();
    for r in &p.rows {
        if !order.contains(&r.id) {
            order.push(r.id.clone());
        }
    }
    assert_eq!(order.len(), 8, "{order:?}");
    let g = Goal {
        target: Some(id_of(&top, "Default (recommended)")),
        ..Default::default()
    };
    assert_eq!(next_step(&p, &g, &order), Step::Key("up".into()));
    let lost = Goal {
        target: Some("not-a-model".into()),
        ..Default::default()
    };
    assert!(matches!(next_step(&p, &lost, &order), Step::Fail(_)));
}

#[test]
fn planning_adjuster() {
    let p = picker(Harness::Claude, "claude/2.1.295/effort.txt").unwrap();
    let to = |v: &str| Goal {
        adjust: Some(v.into()),
        ..Default::default()
    };
    assert_eq!(step(&p, &to("max")), Step::Key("right".into()));
    assert_eq!(step(&p, &to("low")), Step::Key("left".into()));
    assert_eq!(step(&p, &to("medium")), Step::Commit("s".into()));
    // `q0` on an adjuster-only picker means the same thing.
    let q0 = Goal {
        target: Some("high".into()),
        ..Default::default()
    };
    assert_eq!(step(&p, &q0), Step::Key("right".into()));
    assert!(matches!(step(&p, &to("ultra")), Step::Fail(_)));
    // The model picker: walk the pointer first, then the adjuster.
    let m = picker(Harness::Claude, "claude/2.1.295/model.txt").unwrap();
    let both = Goal {
        target: Some(id_of(&m, "Fable 5.1")),
        adjust: Some("high".into()),
        ..Default::default()
    };
    assert_eq!(step(&m, &both), Step::Key("down".into()));
    let here = Goal {
        target: Some(id_of(&m, "Opus 5.5")),
        adjust: Some("high".into()),
        ..Default::default()
    };
    assert_eq!(step(&m, &here), Step::Key("right".into()));
}

#[test]
fn planning_multi_select_toggles_then_commits() {
    let p = generic_menu(MULTI).unwrap();
    let want = |ids: &[&str]| Goal {
        checked: Some(ids.iter().map(|s| s.to_string()).collect()),
        ..Default::default()
    };
    // Pointer on Lint (checked) and Lint must be unchecked: toggle here.
    assert_eq!(step(&p, &want(&["type-check"])), Step::Key("space".into()));
    // Lint stays; Unit tests must be checked: move down first.
    assert_eq!(
        step(&p, &want(&["lint", "unit-tests", "type-check"])),
        Step::Key("down".into())
    );
    // Already the desired set: commit with the footer's confirm key.
    assert_eq!(
        step(&p, &want(&["lint", "type-check"])),
        Step::Commit("enter".into())
    );
}

#[test]
fn planning_cancel_and_unknown() {
    let p = picker(Harness::Codex, "codex/0.161.0/resume.txt").unwrap();
    let cancel = Goal {
        cancel: true,
        ..Default::default()
    };
    assert_eq!(step(&p, &cancel), Step::Commit("escape".into()));
    let pick = Goal {
        target: Some("x".into()),
        ..Default::default()
    };
    assert!(matches!(step(&p, &pick), Step::Fail(_)));
}

#[test]
fn screen_questions_walk_instead_of_digits() {
    const Q: &str = "╭──────────────────────────────────────────────╮
│ Bash command                                 │
│                                              │
│   rm -rf build                               │
│                                              │
│ Do you want to proceed?                      │
│ ❯ 1. Keep going                              │
│   2. Stop here                               │
╰──────────────────────────────────────────────╯";
    let d = screen::evaluate(Harness::Claude, Q).dialog.unwrap();
    assert_eq!(d.kind, InteractionKind::Question);
    let it = Interaction {
        kind: InteractionKind::Question,
        questions: d.options_as_question(),
        ..super::harness_tests_blank()
    };
    assert!(super::screen_walk::walks(Harness::Claude, &d, &it));
    let view = super::screen_walk::question_view(&d).unwrap();
    let answer = Answer {
        choices: vec![("q0".into(), vec!["Stop here".into()])],
        ..Default::default()
    };
    let g = super::screen_walk::goal_of(&it, &answer);
    assert_eq!(g.target.as_deref(), Some("2"));
    assert_eq!(step(&view, &g), Step::Key("down".into()));
}

#[test]
fn json_shape_of_a_picker_interaction() {
    let p = picker(Harness::Codex, "codex/0.161.0/model.txt").unwrap();
    let it = Interaction {
        kind: InteractionKind::Picker,
        title: p.title.clone(),
        questions: screen::picker_questions(&p),
        picker: Some(screen::picker_info(&p)),
        ..super::harness_tests_blank()
    };
    let v = serde_json::to_value(&it).unwrap();
    assert_eq!(v["kind"], "Picker");
    assert_eq!(v["picker"]["name"], "model");
    assert_eq!(v["picker"]["cancel_key"], "Escape");
    assert_eq!(v["questions"][0]["options"][1]["selected"], true);
    // Old stores (no `picker`, no `selected`) still parse.
    let mut old = v.clone();
    old.as_object_mut().unwrap().remove("picker");
    old["questions"][0]["options"][0]
        .as_object_mut()
        .unwrap()
        .remove("selected");
    let back: Interaction = serde_json::from_value(old).unwrap();
    assert!(back.picker.is_none());
}

// ---- review findings: option identity and signatures ------------------------------------------

/// Review finding: in a scrolling list an option id names the same row in every scroll window
/// (old ids were label slugs numbered per window: `hello-2` became `hello` after a scroll).
#[test]
fn scrolling_list_ids_survive_scrolling() {
    let top = picker(Harness::Claude, "claude/2.1.295/model.txt").unwrap();
    let down = picker(Harness::Claude, "claude/2.1.295/model-scrolled.txt").unwrap();
    for l in ["Opus 5.5", "Fable 5.1", "Sonnet 5"] {
        assert_eq!(id_of(&top, l), id_of(&down, l), "{l}");
    }
    let w1 = generic_menu(&[
        "Pick a session",
        "",
        "❯ hello  main · 2KB",
        "  hello  dev · 9KB",
        "  ↓ 3 more",
        "",
        "↑/↓ to move · Enter to select · Esc to cancel",
    ])
    .unwrap();
    let w2 = generic_menu(&[
        "Pick a session",
        "",
        "↑ hello  dev · 9KB",
        "❯ zeta  dev · 1KB",
        "  ↓ 2 more",
        "",
        "↑/↓ to move · Enter to select · Esc to cancel",
    ])
    .unwrap();
    assert!(w1.scrolls && w2.scrolls);
    assert_eq!(w1.signature, w2.signature, "scrolling is navigation");
    let dev = w1.rows[1].id.clone();
    assert_ne!(dev, w1.rows[0].id);
    assert_eq!(w2.rows[0].id, dev, "the same row keeps its id");
    // An answer for `main · 2KB` (scrolled out of view) never commits the `dev` row.
    let g = Goal {
        target: Some(w1.rows[0].id.clone()),
        ..Default::default()
    };
    let order: Vec<String> = w1
        .rows
        .iter()
        .chain(&w2.rows[1..])
        .map(|r| r.id.clone())
        .collect();
    assert_eq!(next_step(&w2, &g, &order), Step::Key("up".into()));
}

/// Review finding: rows that read the same (label and description) cannot be told apart on
/// screen, so answering one of them is refused, never guessed.
#[test]
fn twin_rows_are_refused() {
    let p = generic_menu(&[
        "Pick a session",
        "",
        "  hello  main · 2KB",
        "❯ hello  main · 2KB",
        "  other  dev · 1KB",
        "  ↓ 3 more",
        "",
        "↑/↓ to move · Enter to select · Esc to cancel",
    ])
    .unwrap();
    assert_ne!(p.rows[0].id, p.rows[1].id, "option ids stay unique");
    for i in [0, 1] {
        let g = Goal {
            target: Some(p.rows[i].id.clone()),
            ..Default::default()
        };
        assert!(
            matches!(step(&p, &g), Step::Fail(r) if r.starts_with("picker_changed")),
            "row {i}"
        );
    }
    let other = Goal {
        target: Some(p.rows[2].id.clone()),
        ..Default::default()
    };
    assert_eq!(step(&p, &other), Step::Key("down".into()));
}

/// Review finding: a resume row's id holds its stable facts (branch, size) but not its ticking
/// age, and the search filter is part of the signature.
#[test]
fn resume_ids_ignore_the_age_and_the_filter_changes_the_signature() {
    let text = rec("claude/2.1.295/resume.txt");
    let p = screen::evaluate(Harness::Claude, &text)
        .dialog
        .and_then(|d| d.picker)
        .unwrap();
    let later = text.replace("1 second ago", "9 seconds ago");
    let q = screen::evaluate(Harness::Claude, &later)
        .dialog
        .and_then(|d| d.picker)
        .unwrap();
    assert_eq!(p.rows[0].id, q.rows[0].id);
    assert_eq!(p.signature, q.signature);
    assert!(p.rows[0].id.starts_with("fix-flaky-upload-test~"));
    let filtered = text.replace("⌕ Search…", "⌕ fix");
    let f = screen::evaluate(Harness::Claude, &filtered)
        .dialog
        .and_then(|d| d.picker)
        .unwrap();
    assert_ne!(
        p.signature, f.signature,
        "a filtered list is a different list"
    );
}

/// Review finding: two confirmations with the same title and options but a different body or
/// option description are different pickers; moving the pointer is not.
#[test]
fn signature_covers_body_and_descriptions_but_not_navigation() {
    let confirm = |body: &str, yes: &str, at_no: bool| {
        let (y, n) = if at_no {
            ("  ", "❯ ")
        } else {
            ("❯ ", "  ")
        };
        let lines = [
            "Delete the branch?".to_string(),
            body.to_string(),
            String::new(),
            format!("{y}Yes  {yes}"),
            format!("{n}No"),
            String::new(),
            "Enter to confirm · Esc to cancel".to_string(),
        ];
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        generic_menu(&refs).unwrap()
    };
    let a = confirm("Removes feature/a from the remote.", "delete it", false);
    let b = confirm("Removes main from the remote.", "delete it", false);
    let c = confirm(
        "Removes feature/a from the remote.",
        "force-delete it",
        false,
    );
    let moved = confirm("Removes feature/a from the remote.", "delete it", true);
    assert_eq!(
        a.body.as_deref(),
        Some("Removes feature/a from the remote.")
    );
    assert_ne!(a.signature, b.signature, "body");
    assert_ne!(a.signature, c.signature, "option description");
    assert_eq!(a.signature, moved.signature, "pointer");
    // Checkbox state is navigation too.
    let mut m = MULTI.to_vec();
    m[3] = "  [x] Unit tests";
    assert_eq!(
        generic_menu(MULTI).unwrap().signature,
        generic_menu(&m).unwrap().signature
    );
}
