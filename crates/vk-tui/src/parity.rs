//! M4 parity surfaces (08 §2.1 groups, §4 status bar, §5 floats, §7 notifications, copy-mode /
//! global search, theme auto, layout save/apply): shared state and the few hooks `app.rs`
//! calls. Each surface lives in its own module; this one only routes.

use crate::app::{App, Popup, PromptKind, RpcErr};
use crate::screen::Grid;
use serde_json::Value;
use vk_proto::input::KeyEvent;
use vk_proto::render::CursorShape;

#[derive(Default)]
pub struct State {
    pub floats: crate::floats::State,
    pub groups: crate::groups::State,
    pub status: crate::statusbar::State,
    pub search: crate::search::State,
    pub appearance: crate::appearance::State,
    pub notes: crate::notifications::State,
}

/// What a command response is for.
#[derive(Debug, Clone)]
pub enum Reply {
    /// Errors become a toast; results are dropped.
    Ignore,
    /// `status.segments`.
    Status,
    Search(crate::search::Reply),
    /// `group.create`; then move `ws` into the new group.
    GroupCreated {
        ws: Option<String>,
    },
    /// `layout.export {format: toml}` for `layout_save`.
    LayoutExport {
        name: String,
    },
    /// `layout.list` for `layout_apply`.
    LayoutList,
    /// `layout.apply`.
    LayoutApplied {
        name: String,
    },
}

pub fn on_reply(app: &mut App, mi: usize, r: Reply, res: Result<Value, RpcErr>) {
    match r {
        Reply::Ignore => {
            if let Err(e) = res {
                app.toast(format!("✗ {}", e.message));
            }
        }
        Reply::Status => crate::statusbar::on_reply(app, mi, res),
        Reply::Search(r) => crate::search::on_reply(app, mi, r, res),
        Reply::GroupCreated { ws } => crate::groups::on_created(app, mi, ws, res),
        Reply::LayoutExport { name } => crate::layouts::on_export(app, &name, res),
        Reply::LayoutList => crate::layouts::on_list(app, mi, res),
        Reply::LayoutApplied { name } => match res {
            Ok(_) => app.toast(format!("applied layout {name}")),
            Err(e) => app.toast(format!("✗ layout {name}: {}", e.message)),
        },
    }
}

pub fn on_connected(app: &mut App, mi: usize) {
    crate::appearance::on_connected(app, mi);
    app.parity.status.stale = true;
}

pub fn on_model(app: &mut App, _mi: usize) {
    crate::appearance::apply(app, false);
    crate::statusbar::on_model(app);
    crate::floats::on_model(app);
}

pub fn on_tick(app: &mut App) {
    crate::statusbar::tick(app);
    crate::search::poll_jump(app);
}

/// Actions these surfaces own (keymap or palette). True when handled.
pub fn action(app: &mut App, action: &str) -> bool {
    crate::floats::action(app, action)
        || crate::groups::action(app, action)
        || crate::search::action(app, action)
        || crate::layouts::action(app, action)
        || crate::statusbar::action(app, action)
        || crate::appearance::action(app, action)
}

/// Mouse on float frames, sidebar groups and the status bar. True when consumed.
pub fn on_mouse(app: &mut App, me: &crossterm::event::MouseEvent) -> bool {
    crate::floats::on_mouse(app, me)
        || crate::groups::on_mouse(app, me)
        || crate::statusbar::on_mouse(app, me)
}

pub fn popup_key(app: &mut App, ev: KeyEvent, p: Popup) {
    match p {
        Popup::GroupPick { mi, ws, sel } => crate::groups::pick_key(app, ev, mi, ws, sel),
        Popup::Search(s) => crate::search::popup_key(app, ev, s),
        Popup::LayoutPick { mi, layouts, sel } => {
            crate::layouts::pick_key(app, ev, mi, layouts, sel)
        }
        _ => {}
    }
}

pub fn popup_draw(app: &App, g: &mut Grid, p: &Popup) -> Option<(u16, u16, CursorShape)> {
    match p {
        Popup::GroupPick { mi, ws, sel } => {
            crate::groups::draw_pick(app, g, *mi, ws, *sel);
            None
        }
        Popup::Search(s) => crate::search::draw_popup(app, g, s),
        Popup::LayoutPick { layouts, sel, .. } => {
            crate::layouts::draw_pick(app, g, layouts, *sel);
            None
        }
        _ => None,
    }
}

pub fn submit_prompt(app: &mut App, kind: PromptKind, v: String) {
    match kind {
        PromptKind::GroupNew { mi, ws } => crate::groups::create(app, mi, &v, ws),
        PromptKind::GroupRename { mi, group } => crate::groups::rename(app, mi, &group, &v),
        PromptKind::LayoutSave { mi, tab } => crate::layouts::save(app, mi, &tab, &v),
        _ => {}
    }
}
