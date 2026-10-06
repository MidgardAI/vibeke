//! Edit-scrollback viewer: paging `pane.read` backwards, joining soft wraps, positioning,
//! search, the read-only private editor file and the editor argv. Never runs an editor or
//! touches the host terminal.

use super::*;
use crate::drafts::tests::{ch, commands, fleet, named, only, reply, reply_err, screen, typ};
use vk_proto::input::Mods;

fn rows(from: u64, to: u64) -> Value {
    let rows: Vec<Value> = (from..to)
        .map(|n| json!({"n": n, "text": format!("line {n}"), "wrapped": false}))
        .collect();
    json!({"rows": rows})
}

fn page(from: u64, to: u64, first: u64) -> Value {
    let mut v = rows(from, to);
    v["first"] = json!(first);
    v["more_before"] = json!(from > first);
    v
}

#[test]
fn joins_soft_wraps_and_trims() {
    let r = vec![
        (10, "hello ".to_string(), true),
        (11, "world   ".to_string(), false),
        (12, "next".to_string(), false),
        (13, String::new(), false),
    ];
    assert_eq!(
        join_rows(&r),
        vec![
            (10, "hello world".to_string()),
            (12, "next".to_string()),
            (13, String::new())
        ]
    );
}

#[test]
fn loads_pages_backwards_and_opens_at_the_live_screen() {
    let (mut app, mut rx) = fleet();
    app.action("edit_scrollback", None);
    assert!(matches!(app.mode, Mode::Popup(Popup::Scrollback)));
    let (req, p) = only(&commands(&mut rx[0]), "pane.read");
    assert_eq!(p, json!({"pane": "p1", "source": "archive", "lines": PAGE}));
    assert!(screen(&app).contains("loading…"));
    reply(&mut app, 0, req, page(5000, 10000, 2000));
    // Older rows: the next page ends where this one starts.
    let (req, p) = only(&commands(&mut rx[0]), "pane.read");
    assert_eq!(p["to"], 5000);
    reply(&mut app, 0, req, page(2000, 5000, 2000));
    assert!(commands(&mut rx[0]).is_empty());
    let v = app.scrollback.as_ref().unwrap();
    assert!(!v.loading);
    assert_eq!(v.lines.len(), 8000);
    assert_eq!(v.lines[0], (2000, "line 2000".to_string()));
    let s = screen(&app);
    assert!(s.contains("8000 lines · history lines 2000–9999"), "{s}");
    assert!(s.contains("line 9999"), "{s}");
    assert!(!s.contains("line 2000\n"), "{s}");
    assert!(s.contains("read-only"), "{s}");
    // g goes to the oldest line.
    app.on_key(ch('g'));
    assert!(screen(&app).contains("line 2000"));
    app.on_key(named(NamedKey::Escape));
    assert!(app.scrollback.is_none());
    assert!(matches!(app.mode, Mode::Normal));
}

#[test]
fn cap_marks_older_lines_not_loaded_and_errors_show() {
    let (mut app, mut rx) = fleet();
    app.action("edit_scrollback", None);
    let (req, _) = only(&commands(&mut rx[0]), "pane.read");
    // Pretend we're already at the cap.
    app.scrollback.as_mut().unwrap().rows = (0..MAX_ROWS as u64 - 5010)
        .map(|n| (1_000_000 + n, String::new(), false))
        .collect();
    reply(&mut app, 0, req, page(990_000, 995_000, 0));
    let (req, p) = only(&commands(&mut rx[0]), "pane.read");
    assert_eq!(p["lines"], 10);
    reply(&mut app, 0, req, page(989_990, 990_000, 0));
    assert!(commands(&mut rx[0]).is_empty());
    assert!(app.scrollback.as_ref().unwrap().truncated);
    assert!(screen(&app).contains("older lines not loaded"));

    let (mut app, mut rx) = fleet();
    app.action("edit_scrollback", None);
    let (req, _) = only(&commands(&mut rx[0]), "pane.read");
    reply_err(&mut app, 0, req, "permission_denied", json!({}));
    assert!(screen(&app).contains("✗ permission_denied!"));
}

#[test]
fn search_and_goal_positioning() {
    let (mut app, mut rx) = fleet();
    crate::scrollback::open(&mut app, Some(150));
    let (req, _) = only(&commands(&mut rx[0]), "pane.read");
    reply(&mut app, 0, req, page(100, 300, 100));
    assert_eq!(app.scrollback.as_ref().unwrap().top, 50);
    app.on_key(ch('/'));
    typ(&mut app, "LINE 2");
    assert!(screen(&app).contains("/LINE 2"));
    app.on_key(named(NamedKey::Enter));
    // Smart case: uppercase is exact, so nothing matches.
    assert!(screen(&app).contains("not found: LINE 2"));
    app.on_key(ch('/'));
    typ(&mut app, "line 27");
    app.on_key(named(NamedKey::Enter));
    let top = app.scrollback.as_ref().unwrap().top;
    assert_eq!(app.scrollback.as_ref().unwrap().lines[top].1, "line 270");
    app.on_key(ch('n'));
    let top = app.scrollback.as_ref().unwrap().top;
    assert_eq!(app.scrollback.as_ref().unwrap().lines[top].1, "line 271");
    app.on_key(ch('N'));
    let top = app.scrollback.as_ref().unwrap().top;
    assert_eq!(app.scrollback.as_ref().unwrap().lines[top].1, "line 270");
}

#[test]
fn opened_from_copy_mode_at_its_view_via_a_copy_mode_key() {
    let (mut app, mut rx) = fleet();
    let mut cfg = app.config.clone();
    cfg.keys
        .copy_mode
        .overrides
        .insert("ctrl+e".into(), "edit_scrollback".into());
    app.copy_keys =
        std::sync::Arc::new(crate::copykeys::CopyKeys::from_config(&cfg.keys.copy_mode));
    app.enter_copy(None);
    commands(&mut rx[0]);
    app.on_key(KeyEvent::new(Key::Char('e'), Mods::CTRL));
    assert!(matches!(app.mode, Mode::Popup(Popup::Scrollback)));
    assert_eq!(app.scrollback.as_ref().unwrap().goal, Some(0));
    assert_eq!(
        only(&commands(&mut rx[0]), "pane.read").1["source"],
        "archive"
    );
}

#[test]
fn editor_file_is_private_read_only_and_argv_has_the_line() {
    let dir = tempfile::tempdir().unwrap();
    let priv_dir = dir.path().join("vk");
    let f = write_private(&priv_dir, "p:1/x", "a\nb\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&priv_dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(&f).unwrap().permissions().mode() & 0o777,
        0o400
    );
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "a\nb\n");
    assert!(
        f.file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("scrollback-p_1_x-")
    );
    // A shared (group/world-accessible) directory is refused.
    let open_dir = dir.path().join("open");
    std::fs::create_dir(&open_dir).unwrap();
    std::fs::set_permissions(&open_dir, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(write_private(&open_dir, "p1", "x").is_err());
    // Cleanup removes the read-only file.
    remove_file(&f);
    assert!(!f.exists());

    let file = Path::new("/tmp/x.txt");
    let ed = editor_from(Some("nvim -R".into()), Some("nano".into())).unwrap();
    assert_eq!(
        editor_argv(&ed, file, 42),
        vec!["nvim", "-R", "+42", "/tmp/x.txt"]
    );
    let ed = editor_from(Some("  ".into()), Some("/usr/bin/hx".into())).unwrap();
    assert_eq!(
        editor_argv(&ed, file, 7),
        vec!["/usr/bin/hx", "/tmp/x.txt:7"]
    );
    let ed = editor_from(None, Some("code --wait".into())).unwrap();
    assert_eq!(
        editor_argv(&ed, file, 7),
        vec!["code", "--wait", "/tmp/x.txt"]
    );
    assert_eq!(editor_from(None, None), None);
}

#[test]
fn e_queues_the_editor_with_the_text_and_never_runs_it_here() {
    let (mut app, mut rx) = fleet();
    app.action("edit_scrollback", None);
    let (req, _) = only(&commands(&mut rx[0]), "pane.read");
    reply(&mut app, 0, req, page(0, 3, 0));
    let dir = tempfile::tempdir().unwrap();
    app.scrollback.as_mut().unwrap().dir = dir.path().join("priv");
    // Unset-editor handling is covered by `editor_from`; here force one through the env-free
    // path by calling the pieces the key uses.
    let v = app.scrollback.as_ref().unwrap();
    assert_eq!(text_of(v), "line 0\nline 1\nline 2\n");
    let file = write_private(&v.dir, &v.pane, &text_of(v)).unwrap();
    let argv = editor_argv(&["vim".to_string()], &file, v.top + 1);
    assert_eq!(argv[1], "+1");
    remove_file(&file);
    // The key path: with VISUAL/EDITOR absent or present, nothing is executed by `key`; at most
    // `app.external` is queued for the main loop.
    app.on_key(ch('e'));
    if let Some(x) = app.external.take() {
        assert!(x.file.starts_with(dir.path()));
        assert_eq!(
            std::fs::read_to_string(&x.file).unwrap(),
            "line 0\nline 1\nline 2\n"
        );
        remove_file(&x.file);
    } else {
        assert!(screen(&app).contains("Set $VISUAL or $EDITOR"));
    }
}
