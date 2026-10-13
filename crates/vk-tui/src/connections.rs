//! Connections: one full pane-area view over a machine's gateway, with four tabs (`connections`,
//! `prefix+alt+d`, opens on the tab last used this session, Devices at first):
//!
//! - **Devices** ([`crate::devices`]; `devices`, `pair_phone`): your own paired phones and apps.
//! - **People** ([`crate::people`]; `people`, `share_pane`): colleagues you shared a pane or a
//!   workspace with.
//! - **Hosts** ([`crate::sharing`]; `hosts`): peers, invitations, pasting an invitation, the
//!   hosts holding access to this one.
//! - **Handoffs** ([`crate::handoff`]; `handoffs`, `prefix+shift+h`): incoming handoffs and the
//!   ones being sent.
//!
//! Each tab keeps its own module, state, `Popup` variant and stage machine (and with them its
//! replies, ticks and pastes); this module draws the title and the tab strip, routes the actions
//! that open a tab, and switches tabs with tab / shift+tab from a list. While a form, picker or
//! prompt in the tab has the keys (each tab module's `in_form`), tab moves between its fields
//! instead. Up from the top of a list or form focuses the tab strip ([`crate::formkeys::Strip`]). Switching leaves a tab as Esc
//! would (a pending pairing link is cancelled) and opens the next one on the same machine; Esc
//! at a tab's top level closes the view.

use vk_proto::input::{Key, KeyEvent, KeyKind, NamedKey};

use crate::app::{App, Mode};
use crate::drafts::Area;
use crate::formkeys::{LIST_TABS_HINT, STRIP_HINT, Strip, StripKey, tab_step};
use crate::screen::Grid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    Devices,
    People,
    Hosts,
    Handoffs,
}

/// The tabs in strip order.
pub const TABS: [Tab; 4] = [Tab::Devices, Tab::People, Tab::Hosts, Tab::Handoffs];

impl Tab {
    pub fn name(self) -> &'static str {
        match self {
            Tab::Devices => "Devices",
            Tab::People => "People",
            Tab::Hosts => "Hosts",
            Tab::Handoffs => "Handoffs",
        }
    }

    fn step(self, by: i32) -> Tab {
        let i = TABS.iter().position(|t| *t == self).unwrap_or(0) as i32;
        TABS[(i + by).rem_euclid(TABS.len() as i32) as usize]
    }
}

#[derive(Debug, Default)]
pub struct State {
    /// The tab opened last this session: `connections` opens it again.
    pub last: Tab,
    /// The machine the view was opened on (the Handoffs tab lists every machine; its title
    /// names this one).
    pub mi: usize,
    /// The tab strip has the keys: left/right switch tabs, down or enter go back into the tab.
    /// Reached with up from the top of a tab's form or list.
    pub strip: bool,
}

/// Open `tab` on machine `mi`.
pub fn open_tab(app: &mut App, tab: Tab, mi: usize) {
    app.ux.connections.last = tab;
    app.ux.connections.mi = mi;
    match tab {
        Tab::Devices => crate::devices::open_on(app, mi, crate::devices::Stage::List),
        Tab::People => crate::people::open_on(app, mi, None),
        Tab::Hosts => crate::sharing::open_on(app, mi),
        Tab::Handoffs => crate::handoff::open_list(app),
    }
}

/// The actions that open a tab.
pub fn action(app: &mut App, action: &str) -> bool {
    let mi = app.cur;
    match action {
        "connections" => {
            let tab = app.ux.connections.last;
            open_tab(app, tab, mi);
        }
        "devices" => open_tab(app, Tab::Devices, mi),
        "pair_phone" => {
            app.ux.connections.last = Tab::Devices;
            app.ux.connections.mi = mi;
            crate::devices::open_on(app, mi, crate::devices::Stage::PickScope { sel: 0 });
        }
        "people" => open_tab(app, Tab::People, mi),
        // The focused pane, or the one right-clicked in the sidebar.
        "share_pane" => match crate::handoff::take_target(app) {
            Some((mi, pane)) => {
                app.ux.connections.last = Tab::People;
                app.ux.connections.mi = mi;
                crate::people::open_on(app, mi, Some(pane));
            }
            None => app.toast("no focused pane"),
        },
        "hosts" => open_tab(app, Tab::Hosts, mi),
        "handoffs" => open_tab(app, Tab::Handoffs, mi),
        _ => return false,
    }
    true
}

/// A form, picker or prompt in `tab` has the keys: tab keeps its meaning there (the next field).
/// Each tab module says so in its own `in_form`; a list is not a form.
fn in_form(app: &App, tab: Tab) -> bool {
    match tab {
        Tab::Hosts => crate::sharing::in_form(app),
        Tab::People => crate::people::in_form(app),
        Tab::Devices => crate::devices::in_form(app),
        Tab::Handoffs => false,
    }
}

/// The machine `tab` is on.
fn machine_of(app: &App, tab: Tab) -> usize {
    match tab {
        Tab::Devices => app.ux.devices.as_ref().map(|v| v.mi),
        Tab::People => app.ux.people.as_ref().map(|v| v.mi),
        Tab::Hosts => app.ux.sharing.view.as_ref().map(|v| v.mi),
        Tab::Handoffs => None,
    }
    .unwrap_or(app.ux.connections.mi)
}

/// Leave `from` and open `to` on the same machine.
pub fn switch(app: &mut App, from: Tab, to: Tab) {
    let mi = machine_of(app, from);
    match from {
        Tab::Devices => crate::devices::leave(app),
        Tab::People => crate::people::leave(app),
        Tab::Hosts => crate::sharing::leave(app),
        Tab::Handoffs => crate::handoff::leave(app),
    }
    app.mode = Mode::Normal;
    open_tab(app, to, mi);
    app.dirty = true;
}

/// A key in `tab`. On the tab strip (see [`State::strip`]) left/right switch tabs and down,
/// enter or esc go back into the tab. Otherwise tab / shift+tab switch tabs, except in a form,
/// where they move between its fields; anything else is the tab's.
pub fn key(app: &mut App, ev: KeyEvent, tab: Tab) {
    // The popup dispatcher clears the mode before a key; each tab's handler sets it again. The
    // strip handles some keys itself, so it keeps the view open the same way.
    let keep = |app: &mut App| app.mode = Mode::Popup(popup_of(tab));
    if app.ux.connections.strip && ev.kind != KeyKind::Release {
        let mut strip = Strip { focused: true };
        match strip.key(&ev) {
            StripKey::Switch(by) => {
                switch(app, tab, tab.step(by));
                strip.focused = true;
            }
            StripKey::Leave | StripKey::None => keep(app),
        }
        app.ux.connections.strip = strip.focused;
        app.dirty = true;
        return;
    }
    if let Some(by) = tab_step(&ev)
        && !in_form(app, tab)
    {
        switch(app, tab, tab.step(by));
        return;
    }
    // Up from the top of the Handoffs list: the tab strip.
    if tab == Tab::Handoffs
        && ev.kind != KeyKind::Release
        && !ev.mods.ctrl()
        && !ev.mods.alt()
        && matches!(ev.key, Key::Named(NamedKey::Up) | Key::Char('k'))
        && app.ux.handoff.list_sel == 0
    {
        keep(app);
        app.ux.connections.strip = true;
        app.dirty = true;
        return;
    }
    match tab {
        Tab::Devices => crate::devices::key(app, ev),
        Tab::People => crate::people::key(app, ev),
        Tab::Hosts => crate::sharing::key(app, ev),
        Tab::Handoffs => crate::handoff::handoffs_key(app, ev),
    }
}

/// The popup that shows `tab`.
fn popup_of(tab: Tab) -> crate::app::Popup {
    use crate::app::Popup;
    match tab {
        Tab::Devices => Popup::Devices,
        Tab::People => Popup::People,
        Tab::Hosts => Popup::Sharing,
        Tab::Handoffs => Popup::Handoffs,
    }
}

/// The view's area for `tab` on machine `mi`: the title, then the tab strip with `tab`
/// highlighted.
pub(crate) fn area<'a>(app: &App, g: &'a mut Grid, tab: Tab, mi: usize) -> Area<'a> {
    let t = app.theme;
    let label = app.machines.get(mi).map_or("", |m| m.label.as_str());
    let mut a = Area::open(app, g, &format!("Vibeke · Connections · {label}"));
    let y = a.y;
    if y < a.bottom() {
        let end = a.r.x + a.r.w.saturating_sub(1);
        let mut x = a.r.x + 1;
        for (i, tb) in TABS.iter().enumerate() {
            if i > 0 {
                x += a.g.put_str(x, y, " · ", t.dim(), end.saturating_sub(x));
            }
            let strip = app.ux.connections.strip;
            let st = if *tb == tab && strip {
                vk_proto::render::Style {
                    attrs: vk_proto::render::attr::BOLD,
                    ..t.sel(t.accent)
                }
            } else if *tb == tab {
                t.sel(t.accent)
            } else {
                t.text()
            };
            let name = if *tb == tab && strip {
                format!("‹ {} ›", tb.name())
            } else {
                tb.name().to_string()
            };
            x += a.g.put_str(x, y, &name, st, end.saturating_sub(x));
        }
        let hint = if app.ux.connections.strip {
            format!("   {STRIP_HINT}")
        } else if in_form(app, tab) {
            "   (↑ from the top for the tabs)".to_string()
        } else {
            format!("   ({LIST_TABS_HINT} · ↑ from the top for the tabs)")
        };
        a.g.put_str(x, y, &hint, t.dim(), end.saturating_sub(x));
        a.y += 1;
    }
    a.line("", t.text());
    a
}

#[cfg(test)]
#[path = "connections_tests.rs"]
mod tests;
