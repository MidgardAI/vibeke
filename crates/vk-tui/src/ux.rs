//! Batch 2B (TUI and UX, spec 08) hub: state and the hooks `app.rs`, `popups.rs` and `draw.rs`
//! call into, so each feature lives in its own module:
//!
//! - [`crate::onboarding`]: first-run screen and `:setup` (08 §9).
//! - [`crate::trust`]: repo-local `.vibeke/config.toml` review and `policy.trust` (08 §11.1).
//! - [`crate::batch`]: batch view over equivalent approvals (08 §8).
//! - [`crate::fleet`]: fleet grid of every agent on every machine (08 §6.6).
//! - [`crate::tabbar`]: tab overflow arrows, middle-click close, drag to reorder (08 §3).
//! - [`crate::sidebar`]: token rules, pulsing working glyph, width/auto-width/drag resize, nested
//!   task workspaces, the collapsed urgency rail (08 §2).
//! - [`crate::popup_pane`]: `[[keys.command]] type = "popup"`, popup drag/resize and dimming,
//!   `ui.interaction_overlay`, edit-scrollback in a popup (08 §5, §8; 03 §11.3).
//! - [`crate::navkeys`]: navigate-mode `/` filter, `t`, `p`; palette argument prompts (08 §6).
//! - [`crate::mouse_focus`]: `ui.focus_follows_mouse` (08 §5).
//! - [`crate::sync_input`]: synchronized input (08 §5).
//!
//! v1 remainder (spec 08, 09 §3.2):
//!
//! - [`crate::elevate`]: the request review view (`auth.elevate` requests, y/n; `auth.approve`
//!   approved calls, y/a/n).
//! - [`crate::scroll_req`]: `pane.scroll_requested` moves this client's view.
//! - [`crate::tabbar`] `tab_renumber`; [`crate::groups`] group drag reorder.
//! - [`crate::nav`] goto previews and machines; [`crate::scrollback`] `editor_include_ansi`.
//! - [`crate::repo_preview`]: a trusted repo's `[preview]`.
//! - [`crate::taskbadge`]: PR badges and recreate/forget for missing tasks.
//! - [`crate::agent_list`]: every agent on every machine by attention (`prefix+alt+a`).
//!
//! Lane 3A: [`crate::collision`]: shared-checkout collisions (pane badge, sidebar line, popup).
//!
//! Handoffs (16 §15.2): [`crate::handoff`]: the accept overlay, the handoffs list and sending a
//! pane to a paired host.

use crate::app::{App, Popup, RpcErr};
use crate::screen::Grid;
use serde_json::Value;
use std::time::Instant;
use vk_proto::input::KeyEvent;

#[derive(Default)]
pub struct State {
    pub onboarding: Option<crate::onboarding::Flow>,
    pub batch: Option<crate::batch::View>,
    pub fleet: Option<crate::fleet::View>,
    pub tabs: crate::tabbar::State,
    pub sidebar: crate::sidebar::State,
    pub popups: crate::popup_pane::State,
    pub nav: crate::navkeys::State,
    pub hover: crate::mouse_focus::State,
    pub sync: crate::sync_input::State,
    pub trust: crate::trust::State,
    pub elevate: crate::elevate::State,
    pub scroll: crate::scroll_req::State,
    pub tasks: crate::taskbadge::State,
    pub collision: crate::collision::State,
    pub handoff: crate::handoff::State,
}

/// Replies routed back to the 2B modules.
#[derive(Debug, Clone)]
pub enum Reply {
    Fleet(crate::fleet::Reply),
    Trust(crate::trust::Reply),
    Popup(crate::popup_pane::Reply),
    Batch(crate::batch::Reply),
    Elevate(crate::elevate::Reply),
    Tasks(crate::taskbadge::Reply),
    Collision(crate::collision::Reply),
    Handoff(crate::handoff::Reply),
    /// `tab.renumber`.
    Renumber,
}

pub fn on_reply(app: &mut App, mi: usize, r: Reply, res: Result<Value, RpcErr>) {
    match r {
        Reply::Fleet(r) => crate::fleet::on_reply(app, mi, r, res),
        Reply::Trust(r) => crate::trust::on_reply(app, mi, r, res),
        Reply::Popup(r) => crate::popup_pane::on_reply(app, mi, r, res),
        Reply::Batch(r) => crate::batch::on_reply(app, mi, r, res),
        Reply::Elevate(r) => crate::elevate::on_reply(app, mi, r, res),
        Reply::Tasks(r) => crate::taskbadge::on_reply(app, mi, r, res),
        Reply::Collision(r) => crate::collision::on_reply(app, mi, r, res),
        Reply::Handoff(r) => crate::handoff::on_reply(app, mi, r, res),
        Reply::Renumber => crate::tabbar::on_renumbered(app, res),
    }
}

/// After (re)connecting machine `mi`.
pub fn on_connected(app: &mut App, mi: usize) {
    crate::elevate::on_connected(app, mi);
    crate::handoff::on_connected(app, mi);
}

/// Palette / key actions owned by 2B modules.
pub fn action(app: &mut App, action: &str) -> bool {
    crate::onboarding::action(app, action)
        || crate::trust::action(app, action)
        || crate::batch::action(app, action)
        || crate::fleet::action(app, action)
        || crate::sync_input::action(app, action)
        || crate::sidebar::action(app, action)
        || crate::tabbar::action(app, action)
        || crate::elevate::action(app, action)
        || crate::agent_list::action(app, action)
        || crate::taskbadge::action(app, action)
        || crate::collision::action(app, action)
        || crate::handoff::action(app, action)
}

/// `[[keys.command]] when = "agent:<harness>"`: only while the focused pane runs that harness
/// (08 §10.3).
pub fn command_active(app: &App, c: &vk_config::KeyCommand) -> bool {
    let Some(h) = c.when.as_deref().and_then(|w| w.strip_prefix("agent:")) else {
        return true;
    };
    let Some(p) = app.focused_pane() else {
        return false;
    };
    app.m()
        .model
        .runs
        .iter()
        .any(|r| r.pane == p && r.harness == h && r.ended_at_ms.is_none())
}

/// A direct binding whose `when` doesn't hold lets the key through to the pane.
pub fn binding_active(app: &App, action: &str) -> bool {
    match action
        .strip_prefix("command:")
        .and_then(|i| i.parse::<usize>().ok())
    {
        Some(i) => app
            .config
            .keys
            .command
            .get(i)
            .is_none_or(|c| command_active(app, c)),
        None => true,
    }
}

pub fn popup_key(app: &mut App, ev: KeyEvent, p: Popup) {
    match p {
        Popup::Onboarding => crate::onboarding::key(app, ev),
        Popup::Batch => crate::batch::key(app, ev),
        Popup::Fleet => crate::fleet::key(app, ev),
        Popup::TrustRepo => crate::trust::key(app, ev),
        _ => {}
    }
}

pub fn popup_draw(app: &App, g: &mut Grid, p: &Popup) {
    match p {
        Popup::Onboarding => crate::onboarding::draw(app, g),
        Popup::Batch => crate::batch::draw(app, g),
        Popup::Fleet => crate::fleet::draw(app, g),
        Popup::TrustRepo => crate::trust::draw(app, g),
        _ => {}
    }
}

/// Mouse events 2B handles before the rest: tab drags and clicks, the sidebar border drag and
/// rail, popup frames, focus follows mouse. True when consumed.
pub fn on_mouse(app: &mut App, me: &crossterm::event::MouseEvent) -> bool {
    if crate::popup_pane::on_mouse(app, me)
        || crate::sidebar::on_mouse(app, me)
        || crate::tabbar::on_mouse(app, me)
    {
        return true;
    }
    crate::mouse_focus::on_mouse(app, me);
    false
}

pub fn on_tick(app: &mut App) {
    let now = Instant::now();
    crate::fleet::tick(app, now);
    crate::mouse_focus::tick(app, now);
    crate::trust::tick(app);
    crate::popup_pane::tick(app);
    crate::sync_input::tick(app);
    crate::navkeys::tick(app);
    crate::elevate::tick(app);
    crate::scroll_req::tick(app);
    crate::taskbadge::tick(app, now);
    crate::collision::tick(app, now);
}

pub fn deadlines(app: &App, now: Instant, d: &mut crate::deadline::Deadlines) {
    crate::fleet::deadlines(app, now, d);
    crate::mouse_focus::deadlines(app, d);
    crate::sidebar::deadlines(app, now, d);
    crate::elevate::deadlines(app, d);
    crate::taskbadge::deadlines(app, d);
    crate::collision::deadlines(app, d);
}

/// Navigate-mode keys added by 2B (true when handled).
pub fn navigate_key(app: &mut App, ev: &KeyEvent, sel: usize) -> bool {
    crate::navkeys::navigate_key(app, ev, sel) || crate::taskbadge::navigate_key(app, ev, sel)
}
