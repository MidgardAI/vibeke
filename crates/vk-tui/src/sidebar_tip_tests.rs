use super::*;
use crate::drafts::tests::{fleet, screen};
use crate::event::KeyModifiers;
use vk_proto::input::{Key, KeyEvent, Mods};

const LONG: &str = "a workspace name much longer than the sidebar is wide";

fn moved(app: &mut App, column: u16, row: u16) {
    app.on_mouse(MouseEvent {
        kind: MouseEventKind::Moved,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    });
}

/// A fleet whose workspace row is cut off; returns that row's screen line.
fn setup() -> (App, u16) {
    let (mut app, _rxs) = fleet();
    app.sidebar_w = 28;
    app.machines[0].model.workspaces[0].name = Some(LONG.into());
    let i = crate::draw::sidebar_rows(&app)
        .iter()
        .position(|r| r.text().contains("a workspace name"))
        .expect("workspace row");
    (app, i as u16 + 1)
}

/// Pretend the pointer has rested long enough.
fn rested(app: &mut App) {
    if let Some((_, since)) = app.ux.tip.hover.as_mut() {
        *since -= DELAY * 2;
    }
}

#[test]
fn words_wrap_and_long_words_break() {
    assert_eq!(
        wrap_words("claude and codex both edited src/auth.ts", 16),
        vec!["claude and codex", "both edited", "src/auth.ts"]
    );
    assert_eq!(wrap_words("abcdefgh", 3), vec!["abc", "def", "gh"]);
    assert_eq!(wrap_words("", 10), vec![""]);
    assert_eq!(
        wrap_capped("one two three four five six", 9, 2),
        vec!["one two", "three…"]
    );
}

#[test]
fn a_cut_off_row_ends_in_an_ellipsis() {
    let (app, y) = setup();
    let s = screen(&app);
    let line = s.lines().nth(y as usize).unwrap();
    let side: String = line.chars().take(app.sidebar_w as usize).collect();
    assert!(side.ends_with('…'), "{side:?}");
    assert!(!s.contains(LONG), "the whole name is not on screen yet");
}

#[test]
fn resting_the_pointer_on_a_cut_off_row_shows_its_whole_text() {
    let (mut app, y) = setup();
    moved(&mut app, 3, y);
    assert!(app.ux.tip.hover.is_some());
    assert!(!screen(&app).contains(LONG), "not before the delay");
    rested(&mut app);
    let s = screen(&app);
    assert!(s.contains(LONG), "{s}");
    // A key hides it, as does moving off the row.
    app.on_key(KeyEvent::new(Key::Char('x'), Mods::empty()));
    assert!(app.ux.tip.hover.is_none());
    moved(&mut app, 3, y);
    rested(&mut app);
    moved(&mut app, 3, y + 1);
    assert!(!screen(&app).contains(LONG));
}

#[test]
fn rows_that_fit_and_the_pane_area_arm_nothing() {
    let (mut app, y) = setup();
    app.sidebar_w = 48;
    app.machines[0].model.workspaces[0].name = Some("api".into());
    moved(&mut app, 3, y);
    assert!(app.ux.tip.hover.is_none());
    moved(&mut app, 60, 5);
    assert!(app.ux.tip.hover.is_none());
}

#[test]
fn navigate_mode_shows_the_selected_rows_whole_text() {
    let (mut app, _) = setup();
    let sel = crate::draw::sidebar_rows(&app)
        .iter()
        .filter(|r| r.selectable())
        .position(|r| r.text().contains("a workspace name"))
        .expect("selectable workspace row");
    app.mode = Mode::Navigate { sel };
    assert!(screen(&app).contains(LONG));
}

#[test]
fn the_tooltip_stays_beside_a_right_sidebar() {
    let (mut app, y) = setup();
    app.config.ui.sidebar.position = vk_config::SidebarPosition::Right;
    let x = crate::chrome::sidebar_x(&app).unwrap();
    moved(&mut app, x + 2, y);
    rested(&mut app);
    let mut g = Grid::new(app.size.0, app.size.1);
    let o = draw(&app, &mut g).expect("tooltip");
    assert!(o.x + o.w <= x, "{o:?} left of the sidebar at {x}");
}

#[test]
fn the_delay_wakes_the_client_once_and_repaints() {
    let (mut app, y) = setup();
    moved(&mut app, 3, y);
    let now = Instant::now();
    assert!(app.deadlines(now).names().contains(&"sidebar_tip"));
    rested(&mut app);
    app.dirty = false;
    app.on_tick();
    assert!(app.dirty, "the tooltip is drawn when its delay passes");
    assert!(
        !app.deadlines(Instant::now())
            .names()
            .contains(&"sidebar_tip"),
        "armed once, not a busy loop"
    );
}

#[test]
fn a_shrunk_agent_name_shows_whole_with_its_status_on_its_own_line() {
    const TITLE: &str = "Fix the flaky login redirect in staging";
    let (mut app, _rxs) = fleet();
    app.size = (120, 30);
    app.sidebar_w = 28;
    app.machines[0].model.runs[0].title = Some(TITLE.into());
    let rows = crate::draw::sidebar_rows(&app);
    let i = rows
        .iter()
        .position(|r| r.target.is_some() && r.text().contains("Fix the flaky"))
        .expect("agent row");
    let status: String = rows[i].segs[2..].iter().map(|(s, _)| s.as_str()).collect();
    let status = status.trim().to_string();
    let y = i as u16 + 1;
    let row_line = screen(&app).lines().nth(y as usize).unwrap().to_string();
    let side: String = row_line.chars().take(28).collect();
    assert!(side.contains('…'), "{side:?}");
    moved(&mut app, 3, y);
    rested(&mut app);
    let s = screen(&app);
    let lines: Vec<String> = s
        .lines()
        .map(|l| l.chars().skip(29).collect::<String>())
        .collect();
    let at = lines
        .iter()
        .position(|l| l.contains(TITLE))
        .unwrap_or_else(|| panic!("{s}"));
    assert!(
        lines[at + 1].contains(&status),
        "{:?} {status:?}",
        lines[at + 1]
    );
    // The box stays on the screen.
    assert!(s.lines().all(|l| l.chars().count() <= 120));
}
