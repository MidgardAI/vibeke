//! `ui.focus_follows_mouse` (08 §5): hovering a pane focuses it after
//! `ui.focus_follows_mouse_delay_ms` (default 120). Never while a popup, card, prompt or any
//! other mode is open, never into or out of a plugin/user popup, and only for pointer motion
//! (the hover is cancelled when the pointer leaves the pane or another event changes the mode).

use crate::app::{App, Mode};
use crate::event::{MouseEvent, MouseEventKind};
use crate::time::{Duration, Instant};

#[derive(Debug, Default, Clone)]
pub struct State {
    /// Pane under the pointer (not the focused one) and since when.
    pub hover: Option<(String, Instant)>,
}

fn delay(app: &App) -> Duration {
    Duration::from_millis(app.config.ui.focus_follows_mouse_delay_ms as u64)
}

fn eligible(app: &App) -> bool {
    app.config.ui.focus_follows_mouse
        && matches!(app.mode, Mode::Normal)
        && crate::plugins::popup(app).is_none()
}

pub fn on_mouse(app: &mut App, me: &MouseEvent) {
    if !eligible(app) {
        app.ux.hover.hover = None;
        return;
    }
    if !matches!(me.kind, MouseEventKind::Moved) {
        return;
    }
    let under = app
        .pane_rects()
        .into_iter()
        .find(|(_, r)| r.contains(me.column, me.row))
        .map(|(p, _)| p);
    match under {
        Some(p) if app.focused_pane().as_deref() != Some(p.as_str()) => {
            if app.ux.hover.hover.as_ref().is_none_or(|(h, _)| *h != p) {
                app.ux.hover.hover = Some((p, Instant::now()));
            }
        }
        _ => app.ux.hover.hover = None,
    }
}

pub fn tick(app: &mut App, now: Instant) {
    let Some((pane, since)) = app.ux.hover.hover.clone() else {
        return;
    };
    if !eligible(app) {
        app.ux.hover.hover = None;
        return;
    }
    if now.duration_since(since) >= delay(app) {
        app.ux.hover.hover = None;
        let cur = app.cur;
        if app.focused_pane().as_deref() != Some(pane.as_str()) {
            app.focus_pane(cur, &pane);
        }
    }
}

pub fn deadlines(app: &App, d: &mut crate::deadline::Deadlines) {
    if let Some((_, since)) = &app.ux.hover.hover {
        d.at("focus_follows_mouse", *since + delay(app));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drafts::tests::fleet;
    use crate::event::KeyModifiers;

    fn moved(app: &mut App, column: u16, row: u16) {
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        });
    }

    #[test]
    fn hover_focuses_after_the_delay_only_when_enabled() {
        let (mut app, _rx) = fleet();
        let r2 = app
            .pane_rects()
            .into_iter()
            .find(|(p, _)| p == "p2")
            .unwrap()
            .1;
        // Off by default: nothing happens.
        moved(&mut app, r2.x + 1, r2.y + 1);
        assert!(app.ux.hover.hover.is_none());
        app.config.ui.focus_follows_mouse = true;
        moved(&mut app, r2.x + 1, r2.y + 1);
        let since = app.ux.hover.hover.as_ref().unwrap().1;
        assert_eq!(
            app.deadlines(since).get("focus_follows_mouse"),
            Some(since + Duration::from_millis(120))
        );
        tick(&mut app, since + Duration::from_millis(50));
        assert_eq!(app.focused_pane().as_deref(), Some("p1"), "not yet");
        tick(&mut app, since + Duration::from_millis(130));
        assert_eq!(app.focused_pane().as_deref(), Some("p2"));
        assert!(app.ux.hover.hover.is_none());
    }

    #[test]
    fn never_while_a_popup_or_mode_is_open() {
        let (mut app, _rx) = fleet();
        app.config.ui.focus_follows_mouse = true;
        let r2 = app
            .pane_rects()
            .into_iter()
            .find(|(p, _)| p == "p2")
            .unwrap()
            .1;
        app.action("help", None);
        moved(&mut app, r2.x + 1, r2.y + 1);
        assert!(app.ux.hover.hover.is_none());
        app.mode = Mode::Normal;
        moved(&mut app, r2.x + 1, r2.y + 1);
        assert!(app.ux.hover.hover.is_some());
        // A mode opening before the delay cancels it.
        app.action("help", None);
        tick(&mut app, Instant::now() + Duration::from_secs(1));
        assert_eq!(app.focused_pane().as_deref(), Some("p1"));
        assert!(app.ux.hover.hover.is_none());
    }
}
