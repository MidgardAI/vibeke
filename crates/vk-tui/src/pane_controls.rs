//! User-invoked pane moves and layout actions. The picker captures IDs, not live focus.
use crate::app::{App, Mode, Pending, Popup};
use crate::screen::Grid;
use crate::time::{Duration, Instant};
use serde_json::json;
use vk_proto::input::{Key, KeyEvent, KeyKind, NamedKey};

#[derive(Default)]
pub struct State {
    pub repeat_until: Option<Instant>,
}

#[derive(Debug, Clone)]
pub struct Picker {
    pub machine: usize,
    pub pane: String,
    pub workspace: String,
    pub source_tab: String,
    pub filter: String,
    pub selected: usize,
    /// Destination tab and anchor, captured when the user selects the row.
    pub destination: Option<(String, String)>,
    pub direction: &'static str,
}

fn resize_direction(action: &str) -> Option<&'static str> {
    Some(match action {
        "resize_pane_left" => "left",
        "resize_pane_right" => "right",
        "resize_pane_up" => "up",
        "resize_pane_down" => "down",
        _ => return None,
    })
}

/// Only repeated resize chords are captured. Any other input ends the repeat window
/// and is processed normally. No unmodified application arrows are intercepted.
pub fn repeat_key(app: &mut App, ev: &KeyEvent) -> bool {
    let Some(until) = app.ux.pane_controls.repeat_until else {
        return false;
    };
    if !matches!(app.mode, Mode::Normal) || Instant::now() >= until {
        app.ux.pane_controls.repeat_until = None;
        return false;
    }
    if let crate::keymap::Resolve::Exact(b) = app.keymap.resolve(&[], ev)
        && resize_direction(&b.action).is_some()
    {
        let action = b.action.clone();
        if ev.kind != KeyKind::Release {
            app.action(&action, None);
        }
        return true;
    }
    if matches!(
        ev.key,
        Key::Named(
            NamedKey::LeftControl
                | NamedKey::RightControl
                | NamedKey::LeftShift
                | NamedKey::RightShift
                | NamedKey::LeftAlt
                | NamedKey::RightAlt
                | NamedKey::LeftSuper
                | NamedKey::RightSuper
        )
    ) {
        return true;
    }
    if ev.kind != KeyKind::Release {
        app.ux.pane_controls.repeat_until = None;
    }
    false
}

pub fn is_action(action: &str) -> bool {
    vk_config::PANE_CONTROL_ACTIONS.contains(&action)
}

pub fn supported(app: &App) -> bool {
    app.m().features.iter().any(|f| f == "pane_controls")
}

pub fn action(app: &mut App, action: &str) -> bool {
    if action == "resize_mode" {
        if crate::floats::focused_float(app).is_some() || supported(app) {
            return false;
        }
        app.toast("Update the connected host to use pane resize controls");
        return true;
    }
    if !is_action(action) {
        return false;
    }
    if !matches!(action, "break_pane" | "move_pane") && !supported(app) {
        app.toast("Update the connected host to use these pane controls");
        return true;
    }
    let Some(pane) = app.focused_pane() else {
        return true;
    };
    let Some(tab) = app.focused_tab() else {
        return true;
    };
    if !tab.layout.contains(&pane) {
        app.toast("Embed the floating pane before rearranging it");
        return true;
    }
    if let Some(dir) = resize_direction(action) {
        app.command(
            "pane.resize",
            json!({"pane": pane, "direction": dir, "percent": 5, "mode": "size", "cols": app.pane_area().w.max(1), "rows": app.pane_area().h.max(1)}),
            Pending::Ignore,
        );
        app.ux.pane_controls.repeat_until = (app.config.keys.resize_repeat_ms > 0).then(|| {
            Instant::now() + Duration::from_millis(app.config.keys.resize_repeat_ms as u64)
        });
        return true;
    }
    match action {
        "break_pane" => {
            if tab.layout.panes().len() == 1 {
                app.toast("This pane already has its own tab");
            } else {
                app.command(
                    "pane.move",
                    json!({"pane": pane, "to": {"new_tab_in": tab.workspace}, "focus": true}),
                    Pending::Ignore,
                );
            }
        }
        "move_pane" => {
            let picker = Picker {
                machine: app.cur,
                pane,
                workspace: tab.workspace,
                source_tab: tab.id,
                filter: String::new(),
                selected: 0,
                destination: None,
                direction: "right",
            };
            if rows(app, &picker).is_empty() {
                app.toast("No other tab in this workspace. Use Move pane to its own tab.");
            } else {
                app.mode = Mode::Popup(Popup::MovePane(picker));
            }
        }
        "toggle_split" => app.command("pane.toggle_split", json!({"pane": pane}), Pending::Ignore),
        "swap_pane_previous" | "swap_pane_next" => app.command(
            "pane.swap",
            json!({"pane": pane,
            "direction": if action == "swap_pane_previous" { "previous" } else { "next" }}),
            Pending::Ignore,
        ),
        "cycle_layout" => app.command(
            "pane.arrange",
            json!({"pane": pane, "layout": "next"}),
            Pending::Ignore,
        ),
        _ => unreachable!(),
    }
    true
}

fn rows(app: &App, p: &Picker) -> Vec<(String, String, String)> {
    let Some(machine) = app.machines.get(p.machine) else {
        return vec![];
    };
    let mut tabs: Vec<_> = machine
        .model
        .tabs
        .iter()
        .filter(|t| t.workspace == p.workspace && t.id != p.source_tab)
        .collect();
    tabs.sort_by(|a, b| a.order.total_cmp(&b.order));
    tabs.into_iter()
        .filter_map(|t| {
            let label = format!("{}  {}", t.number, t.title.as_deref().unwrap_or(&t.handle));
            let anchor = t
                .focused_pane
                .clone()
                .filter(|id| t.layout.contains(id))
                .or_else(|| t.layout.panes().first().cloned())?;
            label
                .to_lowercase()
                .contains(&p.filter.to_lowercase())
                .then(|| (t.id.clone(), anchor, label))
        })
        .collect()
}

pub fn key(app: &mut App, ev: KeyEvent, mut p: Picker) {
    if ev.kind == KeyKind::Release {
        app.mode = Mode::Popup(Popup::MovePane(p));
        return;
    }
    if let Some((tab, anchor)) = &p.destination {
        match ev.key {
            Key::Named(NamedKey::Escape) => p.destination = None,
            Key::Named(NamedKey::Left) | Key::Char('h') => p.direction = "left",
            Key::Named(NamedKey::Right) | Key::Char('l') => p.direction = "right",
            Key::Named(NamedKey::Up) | Key::Char('k') => p.direction = "up",
            Key::Named(NamedKey::Down) | Key::Char('j') => p.direction = "down",
            Key::Named(NamedKey::Enter) => {
                // The server revalidates these captured IDs if another client moved/closed them.
                app.command_on(
                    p.machine,
                    "pane.move",
                    json!({"pane": p.pane, "to": {"tab": tab},
                    "anchor": anchor, "direction": p.direction, "focus": true}),
                    Pending::Ignore,
                );
                return;
            }
            _ => {}
        }
    } else {
        let rows = rows(app, &p);
        p.selected = p.selected.min(rows.len().saturating_sub(1));
        match ev.key {
            Key::Named(NamedKey::Escape) => return,
            Key::Named(NamedKey::Up) => p.selected = p.selected.saturating_sub(1),
            Key::Named(NamedKey::Down) => {
                p.selected = (p.selected + 1).min(rows.len().saturating_sub(1))
            }
            Key::Named(NamedKey::Enter) => {
                if let Some((tab, anchor, _)) = rows.get(p.selected) {
                    p.destination = Some((tab.clone(), anchor.clone()));
                }
            }
            Key::Named(NamedKey::Backspace) => {
                p.filter.pop();
                p.selected = 0;
            }
            Key::Char(c) if !ev.mods.ctrl() && !ev.mods.alt() => {
                p.filter.push(c);
                p.selected = 0;
            }
            _ => {}
        }
    }
    app.mode = Mode::Popup(Popup::MovePane(p));
}

pub fn draw(app: &App, g: &mut Grid, p: &Picker) {
    if let Some((tab, _)) = &p.destination {
        let label = app.machines[p.machine]
            .model
            .tabs
            .iter()
            .find(|t| &t.id == tab)
            .map(|t| t.title.as_deref().unwrap_or(&t.handle))
            .unwrap_or("closed tab");
        let mut b = crate::popups::frame(app, g, 66, 7, "Place pane");
        b.line(&format!("Destination: {label}"), app.theme.text());
        let placement = match p.direction {
            "left" => "Left",
            "right" => "Right",
            "up" => "Above",
            _ => "Below",
        };
        b.line(
            &format!("Placement: {placement} the destination's active pane"),
            app.theme.text(),
        );
        b.line("Arrows or h/j/k/l: placement", app.theme.text());
        b.line("Enter: move   Esc: back", app.theme.text());
    } else {
        let rows = rows(app, p);
        let visible = app.pane_area().h.saturating_sub(8).clamp(1, 12) as usize;
        let selected = p.selected.min(rows.len().saturating_sub(1));
        let start = selected.saturating_sub(visible - 1);
        let mut b = crate::popups::frame(app, g, 66, (visible + 5) as u16, "Move pane to tab");
        b.line(&format!("Filter: {}", p.filter), app.theme.text());
        for (i, (_, _, label)) in rows.iter().enumerate().skip(start).take(visible) {
            b.line(
                &format!("{} {label}", if i == selected { "›" } else { " " }),
                app.theme.text(),
            );
        }
        if rows.is_empty() {
            b.line("No matching tabs", app.theme.text());
        }
        b.line(
            "↑/↓: select   Enter: placement   Esc: cancel",
            app.theme.text(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drafts::tests::{commands, fleet, only};
    use vk_proto::input::Mods;

    fn press(app: &mut App, key: Key, mods: Mods) {
        app.on_key(KeyEvent::new(key, mods));
    }
    fn prefix(app: &mut App) {
        press(app, Key::Char('b'), Mods::CTRL);
    }

    #[test]
    fn native_byte_sequences_and_browser_keys_reach_the_same_actions() {
        for (bytes, method) in [
            (b"\x02!".as_slice(), "pane.move"),
            (b"\x02%".as_slice(), "pane.split"),
            (b"\x02\"".as_slice(), "pane.split"),
            (b"\x02\x1b[1;5D".as_slice(), "pane.resize"),
        ] {
            let (mut app, mut rx) = fleet();
            let mut decoder = crate::input::Decoder::new();
            for input in decoder.feed(bytes) {
                app.on_input(input);
            }
            only(&commands(&mut rx[0]), method);
        }
        // The WASM host sends logical keys with modifiers instead of terminal bytes.
        for (c, method) in [
            ('!', "pane.move"),
            ('%', "pane.split"),
            ('"', "pane.split"),
            ('{', "pane.swap"),
            ('}', "pane.swap"),
        ] {
            let (mut app, mut rx) = fleet();
            prefix(&mut app);
            press(&mut app, Key::Char(c), Mods::SHIFT);
            only(&commands(&mut rx[0]), method);
        }
    }

    #[test]
    fn repeat_ignores_modifier_events_and_passes_escape_to_the_application() {
        let (mut app, mut rx) = fleet();
        app.action("resize_pane_left", None);
        commands(&mut rx[0]);
        let mut release = KeyEvent::new(Key::Named(NamedKey::LeftControl), Mods::empty());
        release.kind = KeyKind::Release;
        app.on_key(release);
        press(&mut app, Key::Named(NamedKey::LeftControl), Mods::CTRL);
        assert!(app.ux.pane_controls.repeat_until.is_some());
        assert!(rx[0].try_recv().is_err());
        press(&mut app, Key::Named(NamedKey::Right), Mods::CTRL);
        only(&commands(&mut rx[0]), "pane.resize");
        press(&mut app, Key::Named(NamedKey::Escape), Mods::empty());
        assert!(app.ux.pane_controls.repeat_until.is_none());
        assert!(matches!(
            rx[0].try_recv().unwrap(),
            vk_proto::render::ClientFrame::Key {
                key: KeyEvent {
                    key: Key::Named(NamedKey::Escape),
                    ..
                },
                ..
            }
        ));
    }

    #[test]
    fn older_hosts_get_an_update_message_instead_of_wrong_resize_semantics() {
        let (mut app, mut rx) = fleet();
        app.machines[0].features.clear();
        for action in [
            "resize_mode",
            "resize_pane_left",
            "toggle_split",
            "cycle_layout",
            "swap_pane_next",
        ] {
            app.action(action, None);
            assert!(
                app.toasts
                    .last()
                    .unwrap()
                    .text
                    .contains("Update the connected host")
            );
            assert!(commands(&mut rx[0]).is_empty());
        }
        app.action("break_pane", None);
        only(&commands(&mut rx[0]), "pane.move");
    }

    #[test]
    fn configured_and_repo_commands_win_over_new_defaults_and_aliases() {
        let (mut app, _) = fleet();
        app.config.keys.command.push(vk_config::KeyCommand {
            key: "prefix+alt+t".into(),
            command: "make test".into(),
            ..Default::default()
        });
        app.keymap = crate::keymap::Keymap::from_config(&app.config);
        assert!(app.keymap.binding_for("toggle_split").is_none());
        assert!(app.keymap.binding_for("command:0").is_some());
        assert!(app.keymap.add_plugin_binding("repo:0:W1:0", "prefix+%"));
        let crate::keymap::Resolve::Exact(b) = app.keymap.resolve(&[], &KeyEvent::ch('%')) else {
            panic!()
        };
        assert_eq!(b.action, "repo:0:W1:0");
        assert!(app.keymap.binding_for("split_vertical").is_some());
    }

    #[test]
    fn resize_mode_sends_size_semantics_and_ignores_releases() {
        let (mut app, mut rx) = fleet();
        app.action("resize_mode", None);
        for key in [
            NamedKey::Left,
            NamedKey::Up,
            NamedKey::Right,
            NamedKey::Down,
        ] {
            press(&mut app, Key::Named(key), Mods::empty());
            let mut release = KeyEvent::new(Key::Named(key), Mods::empty());
            release.kind = KeyKind::Release;
            app.on_key(release);
        }
        let cmds = commands(&mut rx[0]);
        let resize: Vec<_> = cmds.iter().filter(|c| c.1 == "pane.resize").collect();
        assert_eq!(resize.len(), 4);
        for (c, dir) in resize.iter().zip(["left", "up", "right", "down"]) {
            assert_eq!(c.2["mode"], "size");
            assert_eq!(c.2["direction"], dir);
        }
        assert!(matches!(app.mode, Mode::Resize));
        press(&mut app, Key::Named(NamedKey::Escape), Mods::empty());
        assert!(matches!(app.mode, Mode::Normal));
    }
    #[test]
    fn direct_resize_repeats_then_expires_without_eating_application_keys() {
        let (mut app, mut rx) = fleet();
        prefix(&mut app);
        press(&mut app, Key::Named(NamedKey::Left), Mods::CTRL);
        press(&mut app, Key::Named(NamedKey::Right), Mods::CTRL);
        let cmds = commands(&mut rx[0]);
        assert_eq!(cmds.iter().filter(|c| c.1 == "pane.resize").count(), 2);
        app.ux.pane_controls.repeat_until = Some(Instant::now());
        press(&mut app, Key::Named(NamedKey::Left), Mods::CTRL);
        assert!(commands(&mut rx[0]).is_empty());
        prefix(&mut app);
        press(&mut app, Key::Named(NamedKey::Right), Mods::CTRL);
        press(&mut app, Key::Char('x'), Mods::empty());
        assert!(app.ux.pane_controls.repeat_until.is_none());
    }
    #[test]
    fn pane_shortcuts_dispatch_and_picker_cancels_without_mutation() {
        let (mut app, mut rx) = fleet();
        for (key, method) in [
            ('t', "pane.toggle_split"),
            ('{', "pane.swap"),
            ('}', "pane.swap"),
            (' ', "pane.arrange"),
        ] {
            prefix(&mut app);
            press(
                &mut app,
                Key::Char(key),
                if key == 't' { Mods::ALT } else { Mods::empty() },
            );
            let cmds = commands(&mut rx[0]);
            only(&cmds, method);
        }
        let mut tab = app.machines[0].model.tabs[0].clone();
        tab.id = "destination".into();
        tab.number = 2;
        app.machines[0].model.tabs.push(tab);
        app.action("move_pane", None);
        assert!(matches!(app.mode, Mode::Popup(Popup::MovePane(_))));
        press(&mut app, Key::Named(NamedKey::Escape), Mods::empty());
        assert!(commands(&mut rx[0]).is_empty());
        app.action("move_pane", None);
        press(&mut app, Key::Named(NamedKey::Enter), Mods::empty());
        press(&mut app, Key::Named(NamedKey::Down), Mods::empty());
        press(&mut app, Key::Named(NamedKey::Enter), Mods::empty());
        let cmds = commands(&mut rx[0]);
        let cmd = only(&cmds, "pane.move");
        assert_eq!(cmd.1["to"]["tab"], "destination");
        assert_eq!(cmd.1["direction"], "down");
        assert_eq!(cmd.1["focus"], true);
    }
}
