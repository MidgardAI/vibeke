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

#[test]
fn a_long_state_label_does_not_resize_the_sidebar() {
    let (mut app, _rx) = fleet();
    fit(&mut app);
    let w = app.sidebar_w;
    let r = &mut app.machines[0].model.runs[0];
    r.execution.value = Execution::Error;
    r.execution.detail = Some("a very long error message that goes on and on and on and on".into());
    fit(&mut app);
    assert_eq!(app.sidebar_w, w, "the label is cut, not measured");
    let text = crate::draw::agent_row_text(&app, 0, "r1");
    assert!(text.contains("error: a very long"), "{text}");
}

#[test]
fn a_shrinking_sidebar_waits_for_a_clear_gain() {
    let (mut app, _rx) = fleet();
    app.machines[0].model.workspaces[0].name = Some("w".repeat(36));
    fit(&mut app);
    let wide = app.sidebar_w;
    assert!(wide > 28 && wide < 48, "{wide}");
    // A name two columns shorter keeps the width; a much shorter one gives it back.
    app.machines[0].model.workspaces[0].name = Some("w".repeat(34));
    fit(&mut app);
    assert_eq!(app.sidebar_w, wide);
    app.machines[0].model.workspaces[0].name = Some("api".into());
    fit(&mut app);
    assert!(app.sidebar_w < wide);
}

#[test]
fn a_narrow_terminal_hides_the_sidebar_and_a_toggle_opens_it() {
    let (mut app, _rx) = fleet();
    app.size = (70, 30);
    fit(&mut app);
    assert!(!app.sidebar, "under 80 columns the panes get the width");
    app.action("toggle_sidebar", None);
    assert!(app.sidebar);
    fit(&mut app);
    assert!(app.sidebar, "opened by hand: it stays");
    // A wide terminal clears the hand-opened state; a narrow one hides it again.
    app.size = (120, 40);
    fit(&mut app);
    assert!(app.sidebar);
    app.size = (70, 30);
    fit(&mut app);
    assert!(!app.sidebar);
    // Growing again brings the automatically hidden sidebar back.
    app.size = (120, 40);
    fit(&mut app);
    assert!(app.sidebar);
}

#[test]
fn agents_on_a_disconnected_machine_are_dimmed_and_marked() {
    let (mut app, _rx) = fleet();
    app.machines[0].model.runs[0].execution.value = Execution::Working;
    app.machines[0].model.runs[0].execution.since_ms = crate::drafts::now_ms() - 5_000;
    let live = crate::draw::agent_row_text(&app, 0, "r1");
    assert!(
        live.contains("working") && !live.contains("last known"),
        "{live}"
    );
    app.machines[0].tx = None;
    let gone = crate::draw::agent_row_text(&app, 0, "r1");
    assert!(gone.contains("last known"), "{gone}");
    let rows = crate::draw::sidebar_rows(&app);
    let row = rows
        .iter()
        .find(|r| {
            r.target.as_ref().is_some_and(|t| t.1 == "p1")
                && r.segs.iter().any(|(s, _)| s.contains("working"))
        })
        .expect("the agent row");
    assert!(
        row.segs.iter().all(|(_, st)| *st == app.theme.dim()),
        "all segments dim"
    );
}
