//! Connections: one full pane-area view over a machine's gateway, with four tabs (`connections`,
//! `prefix+alt+d`, opens on the tab last used this session, Devices at first):
//!
//! - **Devices** ([`crate::devices`]; `devices`, `pair_phone`): your own paired phones and apps.
//! - **People** ([`crate::people`]; `people`, `share_pane`): colleagues you shared a pane or a
//!   workspace with.
//! - **Hosts** ([`crate::sharing`]; `sharing`): peers, invitations, pasting an invitation, the
//!   hosts holding access to this one.
//! - **Handoffs** ([`crate::handoff`]; `handoffs`, `prefix+shift+h`): incoming handoffs and the
//!   ones being sent.
//!
//! Each tab keeps its own module, state, `Popup` variant and stage machine (and with them its
//! replies, ticks and pastes); this module draws the title and the tab strip, routes the actions
//! that open a tab, and switches tabs with tab / shift+tab, except while a text field in the tab
//! takes the keys (the Hosts paste form, the People form's name). Switching leaves a tab as Esc
//! would (a pending pairing link is cancelled) and opens the next one on the same machine; Esc
//! at a tab's top level closes the view.

use vk_proto::input::{Key, KeyEvent, KeyKind, NamedKey};

use crate::app::{App, Mode};
use crate::drafts::Area;
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

/// The actions that open a tab (old names included, so bindings and habits keep working).
pub fn action(app: &mut App, action: &str) -> bool {
    let mi = app.cur;
    match action {
        "connections" => {
            let tab = app.ux.connections.last;
            open_tab(app, tab, mi);
        }
        "devices" => open_tab(app, Tab::Devices, mi),
        "pair_phone" | "phone_pairing" => {
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
        "sharing" | "sharing_and_handoff" | "invitations" => open_tab(app, Tab::Hosts, mi),
        "handoffs" | "incoming_handoffs" => open_tab(app, Tab::Handoffs, mi),
        _ => return false,
    }
    true
}

/// A text field in `tab` has the keys: tab keeps its meaning there.
fn typing(app: &App, tab: Tab) -> bool {
    match tab {
        Tab::Hosts => crate::sharing::typing(app),
        Tab::People => crate::people::typing(app),
        Tab::Devices | Tab::Handoffs => false,
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

/// A key in `tab`: tab / shift+tab switch tabs, anything else is the tab's.
pub fn key(app: &mut App, ev: KeyEvent, tab: Tab) {
    if ev.kind != KeyKind::Release
        && matches!(ev.key, Key::Named(NamedKey::Tab))
        && !ev.mods.ctrl()
        && !ev.mods.alt()
        && !typing(app, tab)
    {
        let to = tab.step(if ev.mods.shift() { -1 } else { 1 });
        switch(app, tab, to);
        return;
    }
    match tab {
        Tab::Devices => crate::devices::key(app, ev),
        Tab::People => crate::people::key(app, ev),
        Tab::Hosts => crate::sharing::key(app, ev),
        Tab::Handoffs => crate::handoff::handoffs_key(app, ev),
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
            let st = if *tb == tab {
                t.sel(t.accent)
            } else {
                t.text()
            };
            x += a.g.put_str(x, y, tb.name(), st, end.saturating_sub(x));
        }
        a.g.put_str(x, y, "   (tab switches)", t.dim(), end.saturating_sub(x));
        a.y += 1;
    }
    a.line("", t.text());
    a
}

#[cfg(test)]
#[path = "connections_tests.rs"]
mod tests;
