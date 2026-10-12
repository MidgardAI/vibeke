use super::*;

/// The visible rows after feeding `bytes` (`\n` becomes `\r\n`) into an 80×12 engine.
fn screen(bytes: &str) -> Vec<Row> {
    let mut eng = vk_term::Engine::new(80, 12, 100);
    let mut fx = vec![];
    eng.feed(bytes.replace('\n', "\r\n").as_bytes(), &mut fx);
    eng.visible_rows()
}

const RULE: &str = "────────────────────────────────────────";

fn claude(bytes: &str) -> InputBox {
    read(&screen(bytes), Some(Family::Claude))
}

fn codex(bytes: &str) -> InputBox {
    read(&screen(bytes), Some(Family::Codex))
}

#[test]
fn claude_placeholder_is_empty() {
    // The cursor sits inverse on the first character, the rest of the hint is faint.
    let s = format!(
        "● Done.\n\n{RULE}\n❯ \x1b[7mT\x1b[0m\x1b[2mry \"fix lint errors\"\x1b[0m\n{RULE}\n  \x1b[2m? for shortcuts\x1b[0m"
    );
    assert_eq!(claude(&s), InputBox::Empty);
    // A gray placeholder (bright black) counts too.
    let s = format!("{RULE}\n❯ \x1b[90mTry \"refactor the parser\"\x1b[0m\n{RULE}");
    assert_eq!(claude(&s), InputBox::Empty);
    // No placeholder at all, cursor only.
    let s = format!("{RULE}\n❯ \x1b[7m \x1b[0m\n{RULE}");
    assert_eq!(claude(&s), InputBox::Empty);
}

#[test]
fn claude_typed_text_is_a_draft() {
    let s = format!("{RULE}\n❯ fix the bug\x1b[7m \x1b[0m\n{RULE}");
    assert_eq!(claude(&s), InputBox::Draft("fix the bug".into()));
    // A draft that happens to read like the placeholder is still a draft.
    let s = format!("{RULE}\n❯ Try \"fix lint errors\"\n{RULE}");
    assert!(matches!(claude(&s), InputBox::Draft(_)));
    // A continuation line inside the box.
    let s = format!("{RULE}\n❯ \n  second line of my draft\n{RULE}");
    assert!(matches!(claude(&s), InputBox::Draft(t) if t.contains("second line")));
    // A pasted-text chip, even when drawn faint.
    let s = format!("{RULE}\n❯ \x1b[2m[Pasted text #1 +20 lines]\x1b[0m\n{RULE}");
    assert!(matches!(claude(&s), InputBox::Draft(_)));
    // Typed text after a faint chip is typed text.
    let s = format!("{RULE}\n❯ \x1b[2mTry\x1b[0m more\n{RULE}");
    assert!(matches!(claude(&s), InputBox::Draft(_)));
}

#[test]
fn claude_old_rounded_box() {
    assert_eq!(
        claude("╭──────────────────╮\n│ > \x1b[2mTry \"x\"\x1b[0m       │\n╰──────────────────╯"),
        InputBox::Empty
    );
    assert_eq!(
        claude("╭──────────────────╮\n│ > half typed     │\n╰──────────────────╯"),
        InputBox::Draft("half typed".into())
    );
}

#[test]
fn claude_menus_and_open_boxes_are_unknown() {
    // A selection menu is not the input box.
    assert_eq!(
        claude("Do you want to proceed?\n❯ 1. Yes\n  2. No"),
        InputBox::Unknown
    );
    // The box runs off the bottom of the screen.
    assert_eq!(
        claude(&format!("{RULE}\n❯ \x1b[2mTry\x1b[0m")),
        InputBox::Unknown
    );
    assert_eq!(claude("no prompt here"), InputBox::Unknown);
}

#[test]
fn other_layouts_fall_back_to_the_plain_reader() {
    // A bare `> ` line (no rules around it) is empty only when nothing follows the marker.
    assert_eq!(claude("output\n> "), InputBox::Empty);
    assert_eq!(
        claude("output\n> half a thought"),
        InputBox::Draft("half a thought".into())
    );
    // A rounded box whose bottom edge is not drawn yet.
    assert_eq!(claude("\n╭──────────────────╮\n│ > "), InputBox::Empty);
    // The plain reader counts a placeholder as a draft.
    assert!(matches!(
        claude("output\n> \x1b[2mTry \"x\"\x1b[0m"),
        InputBox::Draft(_)
    ));
}

#[test]
fn codex_placeholder_and_footer_are_empty() {
    let s = "• Ran tests\n\n› \x1b[2mAsk Codex to do anything\x1b[0m\n\n  \x1b[2m? for shortcuts                          100% context left\x1b[0m";
    assert_eq!(codex(s), InputBox::Empty);
    // The bold prompt marker and an empty box.
    assert_eq!(
        codex("\x1b[1m›\x1b[0m \n\n  \x1b[2m⏎ send   ⌃J newline\x1b[0m"),
        InputBox::Empty
    );
}

#[test]
fn codex_typed_text_is_a_draft() {
    let s = "› hello world\n\n  \x1b[2m⏎ send   ⌃J newline\x1b[0m";
    assert_eq!(codex(s), InputBox::Draft("hello world".into()));
    // The prompt line is empty, the draft continues below it.
    let s = "› \n  second line\n\n  \x1b[2m? for shortcuts\x1b[0m";
    assert!(matches!(codex(s), InputBox::Draft(t) if t == "second line"));
    // A transcript line further up does not hide the composer below it.
    let s = "› earlier prompt\n• answer\n\n› \x1b[2mAsk Codex to do anything\x1b[0m\n";
    assert_eq!(codex(s), InputBox::Empty);
    assert_eq!(codex("nothing to see"), InputBox::Unknown);
}

#[test]
fn other_harnesses_use_the_plain_reader() {
    assert_eq!(read(&screen("› "), Some(Family::Pi)), InputBox::Empty);
    assert_eq!(
        read(&screen("│ > typed │"), None),
        InputBox::Draft("typed".into())
    );
    assert_eq!(read(&screen("plain"), None), InputBox::Unknown);
}

#[test]
fn faint_styles() {
    let st = |fg: Color, attrs: u16| Style {
        fg,
        attrs,
        ..Default::default()
    };
    assert!(is_faint(&st(Color::Default, attr::DIM)));
    assert!(is_faint(&st(Color::Indexed(8), 0)));
    assert!(is_faint(&st(Color::Rgb(128, 128, 128), 0)));
    assert!(!is_faint(&st(Color::Default, 0)));
    assert!(!is_faint(&st(Color::Rgb(230, 230, 230), 0)));
    assert!(!is_faint(&st(Color::Rgb(217, 119, 87), 0)));
}
