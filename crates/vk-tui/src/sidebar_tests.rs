use super::*;
use crate::app::test_run;
use crate::drafts::tests::{fleet, screen};
use crossterm::event::KeyModifiers;
use vk_config::{SidebarToken, TokenMatch};

fn mouse(app: &mut App, kind: MouseEventKind, column: u16, row: u16) {
    app.on_mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    });
}

fn token(
    harness: Option<&str>,
    state: Option<&str>,
    label: Option<&str>,
    hide: bool,
) -> SidebarToken {
    SidebarToken {
        matcher: TokenMatch {
            harness: harness.map(str::to_string),
            state: state.map(str::to_string),
            regex: None,
        },
        label: label.map(str::to_string),
        color: Some("#ff0000".into()),
        hide,
    }
}

#[test]
fn token_rules_rename_recolour_and_hide() {
    let (mut app, _rx) = fleet();
    assert!(crate::draw::agent_row_text(&app, 0, "r1").contains("claude"));
    app.config.ui.sidebar.token = vec![token(Some("claude"), None, Some("cc"), false)];
    let t = crate::draw::agent_row_text(&app, 0, "r1");
    assert!(t.contains("cc ") && !t.contains("claude"), "{t}");
    let (_, st) = styled_name(&app, 0, &app.machines[0].model.runs[0].clone(), "");
    assert_eq!(st.fg, Color::Rgb(0xff, 0, 0));
    // State matcher: r1 is done (idle, unseen) → a `working` rule doesn't apply.
    app.config.ui.sidebar.token = vec![token(None, Some("working"), Some("busy"), false)];
    assert!(!crate::draw::agent_row_text(&app, 0, "r1").contains("busy"));
    app.config.ui.sidebar.token = vec![token(None, Some("done"), Some("fin"), false)];
    assert!(crate::draw::agent_row_text(&app, 0, "r1").contains("fin"));
    // Regex over name + label; hide drops the row.
    app.config.ui.sidebar.token = vec![SidebarToken {
        matcher: TokenMatch {
            regex: Some("^claude done".into()),
            ..Default::default()
        },
        hide: true,
        ..Default::default()
    }];
    assert!(hidden(&app, 0, &app.machines[0].model.runs[0].clone()));
    assert!(
        crate::draw::sidebar_rows(&app)
            .iter()
            .all(|r| r.target.as_ref().map(|t| t.1.as_str()) != Some("p1")
                || r.segs.iter().any(|(s, _)| s.contains("api"))),
        "only the workspace row targets p1"
    );
}

#[test]
fn working_glyph_pulses_and_rides_the_age_deadline() {
    let (mut app, _rx) = fleet();
    let r = &mut app.machines[0].model.runs[0];
    r.execution.value = Execution::Working;
    r.execution.since_ms = crate::drafts::now_ms();
    let now = Instant::now();
    let d = app.deadlines(now);
    assert!(d.names().contains(&"ages"));
    assert!(!d.names().iter().any(|n| n.contains("pulse")));
    assert!(d.get("ages").unwrap() <= now + std::time::Duration::from_millis(500));
    let base = app.theme.bold(app.theme.accent);
    app.config.ui.animate = false;
    assert_eq!(working_style(&app, base), base);
}

#[test]
fn auto_width_grows_to_fit_and_drag_sets_a_saved_width() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut app, _rx) = fleet();
    app.machines[0].model.workspaces[0].name =
        Some("a-very-long-workspace-name-that-does-not-fit".into());
    fit(&mut app);
    assert!(
        app.sidebar_w > 28 && app.sidebar_w <= 48,
        "{}",
        app.sidebar_w
    );
    app.config.ui.sidebar.auto_width = false;
    fit(&mut app);
    assert_eq!(app.sidebar_w, 28);
    // Drag the border to 22 columns; released → saved.
    load(&mut app, tmp.path().join("sidebar-x.json"));
    let bx = crate::chrome::sidebar_border_x(&app).unwrap();
    mouse(&mut app, MouseEventKind::Down(CtButton::Left), bx, 5);
    mouse(&mut app, MouseEventKind::Drag(CtButton::Left), 22, 5);
    assert_eq!(app.sidebar_w, 22);
    mouse(&mut app, MouseEventKind::Up(CtButton::Left), 22, 5);
    let saved = std::fs::read_to_string(tmp.path().join("sidebar-x.json")).unwrap();
    assert_eq!(saved, r#"{"width":22}"#);
    // Clamped to min_width.
    mouse(&mut app, MouseEventKind::Down(CtButton::Left), 22, 5);
    mouse(&mut app, MouseEventKind::Drag(CtButton::Left), 3, 5);
    assert_eq!(app.sidebar_w, 18);
    mouse(&mut app, MouseEventKind::Up(CtButton::Left), 3, 5);
    // A new client loads it.
    let (mut app2, _rx2) = fleet();
    load(&mut app2, tmp.path().join("sidebar-x.json"));
    fit(&mut app2);
    assert_eq!(app2.sidebar_w, 18);
    app2.action("sidebar_width_reset", None);
    fit(&mut app2);
    assert_eq!(app2.sidebar_w, 28);
}

#[test]
fn task_workspaces_nest_under_their_repo() {
    let (mut app, _rx) = fleet();
    let m = &mut app.machines[0];
    let mut tw = m.model.workspaces[0].clone();
    tw.id = "W2".into();
    tw.name = Some("fix-login".into());
    tw.root_path = "/wt/fix-login".into();
    tw.task = Some("K1".into());
    m.model.workspaces.push(tw);
    m.model.tasks.push(
        serde_json::from_value(serde_json::json!({
            "id": "K1", "handle": "k1", "title": "fix login", "slug": "fix-login",
            "workspace": "W2", "repo_root": "/src/api/", "status": "active", "created_at_ms": 0
        }))
        .unwrap_or_else(|_| vk_proto::model::Task {
            id: "K1".into(),
            repo_root: "/src/api/".into(),
            ..Default::default()
        }),
    );
    m.model
        .panes
        .push(crate::drafts::tests::pane("p9", "T9", "W2"));
    m.model.runs.push(test_run("r9", "p9", "codex"));
    let texts: Vec<String> = crate::draw::sidebar_rows(&app)
        .iter()
        .map(|r| r.segs.iter().map(|(s, _)| s.as_str()).collect())
        .collect();
    let api = texts.iter().position(|t| t.contains("api")).unwrap();
    let task = texts.iter().position(|t| t.contains("fix-login")).unwrap();
    assert!(task > api, "{texts:?}");
    assert!(
        texts[task].starts_with("    "),
        "indented one level: {:?}",
        texts[task]
    );
    assert_eq!(texts.iter().filter(|t| t.contains("fix-login")).count(), 1);
    app.config.ui.sidebar.nest_tasks = false;
    let texts: Vec<String> = crate::draw::sidebar_rows(&app)
        .iter()
        .map(|r| r.segs.iter().map(|(s, _)| s.as_str()).collect())
        .collect();
    let task = texts.iter().find(|t| t.contains("fix-login")).unwrap();
    assert!(
        task.starts_with("  ◆") || task.starts_with("▸ ◆"),
        "{task:?}"
    );
}

#[test]
fn collapsed_sidebar_shows_the_urgency_rail() {
    let (mut app, _rx) = fleet();
    app.action("toggle_sidebar", None);
    assert!(!app.sidebar);
    assert_eq!(crate::chrome::main_x(&app), (2, 118));
    assert_eq!(app.pane_area().x, 2);
    let s = screen(&app);
    let rows: Vec<&str> = s.lines().collect();
    assert!(rows[0].starts_with("≡│"), "{}", rows[0]);
    // r1 is done → ✓ for workspace W1.
    assert!(rows[1].starts_with("✓│"), "{}", rows[1]);
    // Click the top cell: the sidebar opens again.
    mouse(&mut app, MouseEventKind::Down(CtButton::Left), 0, 0);
    assert!(app.sidebar);
    // Right side and the opt-out.
    app.sidebar = false;
    app.config.ui.sidebar.position = vk_config::SidebarPosition::Right;
    assert_eq!(crate::chrome::main_x(&app), (0, 118));
    app.ux.sidebar.hide_rail = true;
    assert_eq!(crate::chrome::main_x(&app), (0, 120));
}

fn bg_task(id: &str, label: &str) -> vk_proto::model::BgTask {
    vk_proto::model::BgTask {
        id: id.into(),
        kind: "shell".into(),
        label: label.into(),
        since_ms: 0,
        source: vk_proto::model::StateSource::Structured,
        tool_use_id: None,
    }
}

#[test]
fn idle_run_with_background_work_is_not_shown_as_done() {
    let (mut app, _rx) = fleet();
    let m = &app.machines[0];
    let (g, _, _, _) = crate::draw::run_state(&app, m, &m.model.runs[0]);
    assert_eq!(g, "✓");
    app.machines[0].model.runs[0].bg_tasks =
        vec![bg_task("b1", "npm run dev"), bg_task("b2", "tail")];
    let m = &app.machines[0];
    let r = &m.model.runs[0];
    let (g, label, _, _) = crate::draw::run_state(&app, m, r);
    assert_eq!(g, "⧗");
    assert_eq!(label, "idle · 2 bg");
    assert_eq!(state_name(&app, m, r), "background");
    assert_eq!(crate::draw::bg_summary(r, 30), "npm run dev, tail");
    app.machines[0].model.runs[0]
        .bg_tasks
        .push(bg_task("b3", "x"));
    let r = &app.machines[0].model.runs[0];
    assert_eq!(crate::draw::bg_summary(r, 30), "npm run dev, tail, +1");
}

#[test]
fn status_segment_counts_background_tasks_and_not_done() {
    let (mut app, _rx) = fleet();
    app.machines[0].model.runs[0].bg_tasks =
        vec![bg_task("b1", "npm run dev"), bg_task("b2", "tail")];
    let d = crate::statusbar::local_data(&app);
    assert_eq!(
        d.pointer("/agents_summary/background"),
        Some(&serde_json::json!(2))
    );
    assert_eq!(
        d.pointer("/agents_summary/done"),
        Some(&serde_json::json!(0))
    );
    let (text, _) = crate::statusbar::segment(&app, "agents_summary", &d);
    assert!(text.contains("⧗2"), "{text}");
}

const LONG_TITLE: &str = "Fix the flaky login redirect in staging";

#[test]
fn a_long_agent_name_shrinks_before_its_state_is_clipped() {
    let (mut app, _rx) = fleet();
    app.sidebar_w = 28;
    app.machines[0].model.runs[0].title = Some(LONG_TITLE.into());
    let rows = crate::draw::sidebar_rows(&app);
    let r = rows
        .iter()
        .find(|r| r.target.is_some() && r.text().contains("Fix the flaky"))
        .expect("agent row");
    // The row keeps the whole title; only the drawn copy shrinks.
    assert!(r.text().contains(LONG_TITLE));
    assert_eq!(r.tip.as_deref(), Some(LONG_TITLE));
    let (segs, shrunk) = r.fitted(28);
    assert!(shrunk);
    let drawn: String = segs.iter().map(|(s, _)| s.as_str()).collect();
    assert!(
        unicode_width::UnicodeWidthStr::width(drawn.as_str()) <= 28,
        "{drawn:?}"
    );
    assert!(
        drawn.contains('…') && !drawn.contains(LONG_TITLE),
        "{drawn:?}"
    );
    // What follows the name (YOLO, state, age) is untouched.
    let status: String = r.segs[2..].iter().map(|(s, _)| s.as_str()).collect();
    assert!(drawn.ends_with(&status), "{drawn:?} vs {status:?}");
    // A wide sidebar shows the row as it is.
    let (segs, shrunk) = r.fitted(120);
    assert!(!shrunk);
    assert_eq!(
        segs.iter().map(|(s, _)| s.as_str()).collect::<String>(),
        r.text()
    );
}

#[test]
fn auto_width_counts_a_long_name_at_the_label_cap() {
    let (mut app, _rx) = fleet();
    app.machines[0].model.runs[0].title = Some("n".repeat(80));
    let rows = crate::draw::sidebar_rows(&app);
    let r = rows
        .iter()
        .find(|r| r.target.is_some() && r.text().contains("nnnn"))
        .expect("agent row");
    let capped = r.auto_width();
    let full = unicode_width::UnicodeWidthStr::width(r.text().as_str());
    assert_eq!(full - capped, 80 - crate::draw::RUN_LABEL_MAX);
}

#[test]
fn truncate_cols_counts_display_columns() {
    use crate::draw::truncate_cols;
    use unicode_width::UnicodeWidthStr;
    assert_eq!(truncate_cols("abcdef", 4), "abc…");
    assert_eq!(truncate_cols("abc", 3), "abc");
    assert_eq!(truncate_cols("", 0), "");
    // CJK characters are two columns wide.
    let cjk = truncate_cols("日本語テキスト", 6);
    assert_eq!(cjk, "日本…");
    assert!(cjk.width() <= 6);
    // An emoji is not split and the result fits.
    let e = truncate_cols("👍👍👍", 4);
    assert_eq!(e, "👍…");
    assert!(e.width() <= 4);
    // A base letter and its combining mark stay together.
    assert_eq!(truncate_cols("e\u{301}e\u{301}e\u{301}", 2), "e\u{301}…");
}
