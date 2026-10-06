//! Sidebar left/right and tab bar top/bottom/hidden: geometry, drawing and hit tests.

use super::*;
use crate::drafts::tests::{commands, fleet, only, screen};
use crossterm::event::{KeyModifiers, MouseButton as CtButton, MouseEvent, MouseEventKind};
use vk_config::BarPosition;
use vk_proto::layout::Rect;

fn rows_of(app: &App) -> Vec<String> {
    screen(app).lines().map(str::to_string).collect()
}

fn click(app: &mut App, column: u16, row: u16) {
    app.on_mouse(MouseEvent {
        kind: MouseEventKind::Down(CtButton::Left),
        column,
        row,
        modifiers: KeyModifiers::NONE,
    });
}

#[test]
fn default_is_sidebar_left_and_tabs_on_top() {
    let (app, _rx) = fleet();
    let w = app.sidebar_w;
    assert_eq!(
        app.pane_area(),
        Rect {
            x: w + 1,
            y: 1,
            w: 120 - w - 1,
            h: 39
        }
    );
    let rows = rows_of(&app);
    assert!(rows[0].starts_with(" vibeke · m0"), "{}", rows[0]);
    assert!(rows[0].contains("1 claude "), "{}", rows[0]);
    assert_eq!(rows[5].chars().nth(w as usize), Some('│'));
}

#[test]
fn sidebar_on_the_right() {
    let (mut app, _rx) = fleet();
    app.config.ui.sidebar.position = vk_config::SidebarPosition::Right;
    let w = app.sidebar_w;
    let area = app.pane_area();
    assert_eq!((area.x, area.w), (0, 120 - w - 1));
    assert_eq!(sidebar_x(&app), Some(120 - w));
    assert_eq!(sidebar_border_x(&app), Some(120 - w - 1));
    assert!(in_sidebar(&app, 119) && in_sidebar(&app, 120 - w));
    assert!(!in_sidebar(&app, 0) && !in_sidebar(&app, 120 - w - 1));
    let rows = rows_of(&app);
    let r0: Vec<char> = rows[0].chars().collect();
    let side: String = r0[(120 - w) as usize..].iter().collect();
    assert!(side.starts_with(" vibeke · m0"), "{side:?}");
    // Tabs start at the left edge now.
    assert!(rows[0].starts_with("  ✓1 claude "), "{}", rows[0]);
    assert_eq!(rows[5].chars().nth((120 - w - 1) as usize), Some('│'));
    // The tiling fills the left part; the sidebar hit test uses the right columns.
    let panes = app.pane_rects();
    assert!(panes.iter().all(|(_, r)| r.x + r.w < 120 - w));
    let (mi, pane) = crate::draw::sidebar_targets(&app)[0].clone();
    let y = (1..40)
        .find(|y| crate::draw::sidebar_hit(&app, *y) == Some((mi, pane.clone())))
        .unwrap();
    app.focus_pane(0, "p2");
    click(&mut app, 119, y);
    assert_eq!(app.focused_pane().as_deref(), Some(pane.as_str()));
}

#[test]
fn tab_bar_at_the_bottom_with_status_bar() {
    let (mut app, mut rx) = fleet();
    app.config.ui.tabs.position = vk_config::TabsPosition::Bottom;
    let area = app.pane_area();
    assert_eq!((area.y, area.h), (0, 39));
    assert_eq!(tab_row(&app), Some(39));
    let rows = rows_of(&app);
    assert!(rows[39].contains("1 claude "), "{}", rows[39]);
    assert!(!rows[0].contains("1 claude "));
    // A click on the bottom row focuses the tab.
    let x = rows[39].find("1 claude ").unwrap() as u16;
    let x = rows[39][..x as usize].chars().count() as u16;
    commands(&mut rx[0]);
    click(&mut app, x, 39);
    let (_, p) = only(&commands(&mut rx[0]), "tab.focus");
    assert_eq!(p["tab"], "T1");
    // Status bar at the bottom sits above the tab bar; at the top it takes row 0.
    app.config.ui.status_bar.enabled = true;
    app.config.ui.status_bar.position = BarPosition::Bottom;
    assert_eq!(crate::statusbar::row(&app), Some(38));
    assert_eq!((app.pane_area().y, app.pane_area().h), (0, 38));
    app.config.ui.status_bar.position = BarPosition::Top;
    assert_eq!(crate::statusbar::row(&app), Some(0));
    assert_eq!((app.pane_area().y, app.pane_area().h), (1, 38));
}

#[test]
fn hidden_tab_bar_keeps_toasts_visible() {
    let (mut app, _rx) = fleet();
    app.config.ui.tabs.position = vk_config::TabsPosition::Hidden;
    assert_eq!(tab_row(&app), None);
    let area = app.pane_area();
    assert_eq!((area.y, area.h), (0, 40));
    let rows = rows_of(&app);
    assert!(!rows.iter().any(|r| r.contains("1 claude ")));
    app.toast("hello there");
    let rows = rows_of(&app);
    assert!(rows[0].trim_end().ends_with("hello there"), "{}", rows[0]);
}

#[test]
fn palette_toggles_placement_for_this_client() {
    let (mut app, _rx) = fleet();
    app.action("sidebar_side", None);
    assert!(sidebar_right(&app));
    app.action("sidebar_side", None);
    assert!(!sidebar_right(&app));
    app.action("tab_bar_position", None);
    assert_eq!(tab_row(&app), Some(39));
    app.action("tab_bar_position", None);
    assert_eq!(tab_row(&app), None);
    app.action("tab_bar_position", None);
    assert_eq!(tab_row(&app), Some(0));
    let entries = crate::nav::palette_entries(&app);
    for id in ["sidebar_side", "tab_bar_position"] {
        assert!(entries.iter().any(|e| e.id == id), "{id}");
    }
}

#[test]
fn config_parses_the_placement_options() {
    let (cfg, _) = vk_config::Config::parse(
        "[ui.sidebar]\nposition = \"right\"\n[ui.tabs]\nposition = \"bottom\"\n",
        std::path::Path::new("config.toml"),
    )
    .unwrap();
    assert_eq!(cfg.ui.sidebar.position, vk_config::SidebarPosition::Right);
    assert_eq!(cfg.ui.tabs.position, vk_config::TabsPosition::Bottom);
}
