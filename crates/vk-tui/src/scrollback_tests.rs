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

// ---- copy_mode.editor_include_ansi --------------------------------------------------------------

fn styled(s: &str, fg: vk_proto::render::Color, attrs: u16) -> vk_proto::render::Row {
    vk_proto::render::Row {
        spans: vec![
            vk_proto::render::Span {
                style: vk_proto::render::Style {
                    fg,
                    attrs,
                    ..Default::default()
                },
                text: s.into(),
                cols: s.chars().count() as u16,
            },
            vk_proto::render::Span {
                style: Default::default(),
                text: "   ".into(),
                cols: 3,
            },
        ],
        ..Default::default()
    }
}

#[test]
fn row_ansi_encodes_styles_trims_and_resets() {
    use vk_proto::render::{Color, attr};
    let r = styled("err", Color::Indexed(1), attr::BOLD);
    assert_eq!(row_ansi(&r, true), "\x1b[0;1;31merr\x1b[0m");
    // Not trimmed inside a wrapped line.
    assert_eq!(row_ansi(&r, false), "\x1b[0;1;31merr\x1b[0m   ");
    // Plain rows carry no escapes; control characters in cells are dropped.
    let mut p = styled("ok\x1b]52;c;x\x07", Color::Default, 0);
    p.spans.truncate(1);
    assert_eq!(row_ansi(&p, true), "ok]52;c;x");
    assert_eq!(
        crate::screen::sgr(vk_proto::render::Style {
            fg: Color::Rgb(1, 2, 3),
            bg: Color::Indexed(200),
            ..Default::default()
        }),
        "\x1b[0;38;2;1;2;3;48;5;200m"
    );
}

#[test]
fn ansi_text_keeps_the_viewer_lines_and_falls_back_to_text() {
    use vk_proto::render::Color;
    let rows = vec![
        (10, "a ".to_string(), true),
        (11, "b".to_string(), false),
        (12, "changed".to_string(), false),
        (13, "plain  ".to_string(), false),
    ];
    let mut map = std::collections::HashMap::new();
    map.insert(10, styled("a ", Color::Indexed(2), 0));
    map.insert(11, styled("b", Color::Indexed(3), 0));
    // The pane scrolled since the archive read: text differs, so the plain text is used.
    map.insert(12, styled("other", Color::Indexed(1), 0));
    let out = ansi_text(&rows, &map);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines.len(),
        join_rows(&rows).len(),
        "same lines as the viewer"
    );
    assert_eq!(lines[0], "\x1b[0;32ma \x1b[0m   \x1b[0;33mb\x1b[0m");
    assert_eq!(lines[1], "changed");
    assert_eq!(lines[2], "plain");
}

#[test]
fn editor_with_ansi_fetches_styled_rows_then_opens() {
    use vk_proto::render::{ClientFrame, Color, ServerFrame};
    let (mut app, mut rx) = fleet();
    app.config.keys.copy_mode.editor_include_ansi = true;
    app.machines[0].panes.get_mut("p1").unwrap().lines =
        vec![styled("line 4", Color::Indexed(2), 0)];
    app.action("edit_scrollback", None);
    let (req, _) = only(&commands(&mut rx[0]), "pane.read");
    let mut p = page(0, 5, 0);
    p["mem_first"] = json!(2);
    reply(&mut app, 0, req, p);
    let dir = tempfile::tempdir().unwrap();
    app.scrollback.as_mut().unwrap().dir = dir.path().join("priv");
    start_editor(&mut app, vec!["vim".into()]);
    assert!(screen(&app).contains("loading colours"));
    let fetch = |rx: &mut tokio::sync::mpsc::UnboundedReceiver<ClientFrame>| {
        let mut v = Vec::new();
        while let Ok(f) = rx.try_recv() {
            if let ClientFrame::FetchHistory {
                req, start, count, ..
            } = f
            {
                v.push((req, start, count));
            }
        }
        v
    };
    let f = fetch(&mut rx[0]);
    assert_eq!(f.len(), 1);
    assert_eq!((f[0].1, f[0].2), (0, 0), "the size first");
    let history = |app: &mut App, req, start, total, lines| {
        app.on_frame(
            0,
            ServerFrame::History {
                pane: "p1".into(),
                req,
                start,
                total,
                lines,
            },
        )
    };
    history(&mut app, f[0].0, 0, 2, vec![]);
    let f = fetch(&mut rx[0]);
    assert_eq!((f[0].1, f[0].2), (0, 2));
    assert!(app.ux.popups.pending_file.is_none(), "not before the rows");
    history(
        &mut app,
        f[0].0,
        0,
        2,
        vec![
            styled("line 2", Color::Indexed(1), 0),
            styled("line 3", Color::Indexed(1), 0),
        ],
    );
    // In a popup on the local machine; the copy has the colours of rows 2-4 only.
    let file = app.ux.popups.pending_file.as_ref().expect("editor opened");
    let text = std::fs::read_to_string(file.path()).unwrap();
    assert_eq!(
        text,
        "line 0\nline 1\n\x1b[0;31mline 2\x1b[0m\n\x1b[0;31mline 3\x1b[0m\n\x1b[0;32mline 4\x1b[0m\n"
    );
    let (_, p) = only(&commands(&mut rx[0]), "pane.float");
    assert_eq!(p["command"][0], "vim");
    assert!(app.scrollback.as_ref().unwrap().ansi.is_none());
}

#[test]
fn editor_without_ansi_or_mem_first_writes_plain_text() {
    let (mut app, mut rx) = fleet();
    app.config.keys.copy_mode.editor_include_ansi = true;
    app.action("edit_scrollback", None);
    let (req, _) = only(&commands(&mut rx[0]), "pane.read");
    reply(&mut app, 0, req, page(0, 2, 0));
    let dir = tempfile::tempdir().unwrap();
    app.scrollback.as_mut().unwrap().dir = dir.path().join("priv");
    // An older server (no mem_first): no fetch, plain text, and it says why.
    start_editor(&mut app, vec!["vim".into()]);
    let file = app.ux.popups.pending_file.as_ref().expect("editor opened");
    assert_eq!(
        std::fs::read_to_string(file.path()).unwrap(),
        "line 0\nline 1\n"
    );
    let msg = app.scrollback.as_ref().unwrap().message.clone().unwrap();
    assert!(msg.contains("without colours"), "{msg}");
    assert!(commands(&mut rx[0]).iter().any(|c| c.1 == "pane.float"));
}
