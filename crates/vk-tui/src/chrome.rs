//! Screen chrome geometry (08 §1, §2.4, §3): where the sidebar (`ui.sidebar.position = "left"
//! | "right"`) and the tab bar (`ui.tabs.position = "top" | "bottom" | "hidden"`) sit, and the
//! horizontal span left for the tab bar, status bar and panes. Every hit test and draw call goes
//! through these, so the placement options are one decision.
//!
//! Palette: `sidebar_side` moves the sidebar left ⇄ right and `tab_bar_position` cycles the tab
//! bar top → bottom → hidden for this client (the config keeps the default).

use crate::app::App;
use crate::screen::Grid;
use vk_config::{SidebarPosition, TabsPosition};

pub fn sidebar_right(app: &App) -> bool {
    app.config.ui.sidebar.position == SidebarPosition::Right
}

/// First column of the sidebar, when shown.
pub fn sidebar_x(app: &App) -> Option<u16> {
    if !app.sidebar {
        return None;
    }
    Some(if sidebar_right(app) {
        app.size.0.saturating_sub(app.sidebar_w)
    } else {
        0
    })
}

/// Column of the sidebar's border (`│`), when shown.
pub fn sidebar_border_x(app: &App) -> Option<u16> {
    let x = sidebar_x(app)?;
    Some(if sidebar_right(app) {
        x.saturating_sub(1)
    } else {
        app.sidebar_w
    })
}

/// Column `x` is in the sidebar (border excluded).
pub fn in_sidebar(app: &App, x: u16) -> bool {
    sidebar_x(app).is_some_and(|s| x >= s && x < s + app.sidebar_w)
}

/// The main area's columns (tab bar, status bar, panes): (first column, width).
pub fn main_x(app: &App) -> (u16, u16) {
    let cols = app.size.0;
    let rail = crate::sidebar::rail_w(app).min(cols);
    match (app.sidebar, sidebar_right(app)) {
        (false, false) => (rail, cols - rail),
        (false, true) => (0, cols - rail),
        (true, false) => {
            let x = (app.sidebar_w + 1).min(cols);
            (x, cols - x)
        }
        (true, true) => (0, cols.saturating_sub(app.sidebar_w + 1)),
    }
}

pub fn tabs_position(app: &App) -> TabsPosition {
    app.config.ui.tabs.position
}

/// Row of the tab bar, when shown.
pub fn tab_row(app: &App) -> Option<u16> {
    match tabs_position(app) {
        TabsPosition::Top => Some(0),
        TabsPosition::Bottom => Some(app.size.1.saturating_sub(1)),
        TabsPosition::Hidden => None,
    }
}

/// Rows the tab bar takes: (top, bottom).
pub fn tab_rows(app: &App) -> (u16, u16) {
    match tabs_position(app) {
        TabsPosition::Top => (1, 0),
        TabsPosition::Bottom => (0, 1),
        TabsPosition::Hidden => (0, 0),
    }
}

/// Palette actions owned here.
pub fn action(app: &mut App, action: &str) -> bool {
    match action {
        "sidebar_side" => {
            let s = &mut app.config.ui.sidebar.position;
            *s = match s {
                SidebarPosition::Left => SidebarPosition::Right,
                SidebarPosition::Right => SidebarPosition::Left,
            };
            app.sidebar = true;
            let side = if sidebar_right(app) { "right" } else { "left" };
            app.toast(format!(
                "sidebar on the {side} (ui.sidebar.position = \"{side}\" to keep it)"
            ));
        }
        "tab_bar_position" => {
            let p = &mut app.config.ui.tabs.position;
            *p = match p {
                TabsPosition::Top => TabsPosition::Bottom,
                TabsPosition::Bottom => TabsPosition::Hidden,
                TabsPosition::Hidden => TabsPosition::Top,
            };
            let name = match tabs_position(app) {
                TabsPosition::Top => "top",
                TabsPosition::Bottom => "bottom",
                TabsPosition::Hidden => "hidden",
            };
            app.toast(format!(
                "tab bar: {name} (ui.tabs.position = \"{name}\" to keep it)"
            ));
        }
        _ => return false,
    }
    // Everything moves: repaint from scratch and resize the panes.
    app.prev = Grid::new(0, 0);
    app.dirty = true;
    true
}

#[cfg(test)]
#[path = "chrome_tests.rs"]
mod tests;
