//! User-invoked popups (08 §6, §8): never opened spontaneously over the focused pane.

use crate::app::{App, Mode, Pending, Popup, PromptKind};
use crate::draw::{harness_icon, run_state};
use crate::screen::{Grid, Rect as SRect};
use crate::time::Instant;
use serde_json::json;
use vk_proto::input::{Key, KeyEvent, KeyKind, NamedKey};
use vk_proto::model::*;
use vk_proto::render::{CursorShape, Style};

pub(crate) struct BoxDraw<'a> {
    g: &'a mut Grid,
    r: SRect,
    y: u16,
}

impl BoxDraw<'_> {
    pub(crate) fn width(&self) -> u16 {
        self.r.w
    }
    /// Write at column `dx` of the current row without advancing; `next` moves on.
    pub(crate) fn put(&mut self, dx: u16, s: &str, st: Style) {
        if self.y + 1 >= self.r.y + self.r.h {
            return;
        }
        let max = self.r.w.saturating_sub(4).saturating_sub(dx);
        if max > 0 {
            self.g.put_str(self.r.x + 2 + dx, self.y, s, st, max);
        }
    }
    pub(crate) fn next(&mut self) {
        self.y += 1;
    }
    /// Rows still free above the bottom border.
    pub(crate) fn rows_left(&self) -> u16 {
        (self.r.y + self.r.h)
            .saturating_sub(1)
            .saturating_sub(self.y)
    }
    pub(crate) fn line(&mut self, s: &str, st: Style) {
        if self.y + 1 >= self.r.y + self.r.h {
            return;
        }
        self.g
            .put_str(self.r.x + 2, self.y, s, st, self.r.w.saturating_sub(4));
        self.y += 1;
    }
}

pub(crate) fn frame<'a>(app: &App, g: &'a mut Grid, w: u16, h: u16, title: &str) -> BoxDraw<'a> {
    let area = app.pane_area();
    let w = w.min(area.w.saturating_sub(2)).max(20);
    let h = h.min(area.h.saturating_sub(1)).max(5);
    let x = area.x + (area.w.saturating_sub(w)) / 2;
    let y = area.y + (area.h.saturating_sub(h)) / 3;
    frame_at(app, g, SRect { x, y, w, h }, title)
}

/// A bordered, titled box at `r` (the caller keeps it inside the pane area).
pub(crate) fn frame_at<'a>(app: &App, g: &'a mut Grid, r: SRect, title: &str) -> BoxDraw<'a> {
    let SRect { x, y, w, h } = r;
    // Remember where popups are on screen this frame (several frames: their union).
    let hit = match app.popup_rect.get() {
        Some(o) => {
            let (x0, y0) = (o.x.min(x), o.y.min(y));
            let (x1, y1) = ((o.x + o.w).max(x + w), (o.y + o.h).max(y + h));
            SRect {
                x: x0,
                y: y0,
                w: x1 - x0,
                h: y1 - y0,
            }
        }
        None => r,
    };
    app.popup_rect.set(Some(hit));
    let t = app.theme;
    let st = t.text();
    g.fill(SRect { x, y, w, h }, st);
    let b = t.border(true);
    for i in x..x + w {
        g.put_str(i, y, "─", b, 1);
        g.put_str(i, y + h - 1, "─", b, 1);
    }
    for j in y..y + h {
        g.put_str(x, j, "│", b, 1);
        g.put_str(x + w - 1, j, "│", b, 1);
    }
    g.put_str(x, y, "┌", b, 1);
    g.put_str(x + w - 1, y, "┐", b, 1);
    g.put_str(x, y + h - 1, "└", b, 1);
    g.put_str(x + w - 1, y + h - 1, "┘", b, 1);
    g.put_str(
        x + 2,
        y,
        &format!(" {title} "),
        t.bold(t.accent),
        w.saturating_sub(4),
    );
    BoxDraw {
        g,
        r: SRect { x, y, w, h },
        y: y + 1,
    }
}

/// Popups are modal for the mouse (the gateway overlay, plugin popups, the preview manager and
/// the scrollback viewer handle their own clicks first). While one is open nothing reaches the
/// panes, sidebar or tab bar; a press outside it closes it the way `esc` does. True when the
/// event was taken.
pub fn on_mouse(app: &mut App, me: &crate::event::MouseEvent) -> bool {
    use crate::event::MouseEventKind;
    if !matches!(app.mode, Mode::Popup(_)) {
        return false;
    }
    if let MouseEventKind::Down(_) = me.kind {
        // Popups drawn over the whole pane area (inbox, drafts, task view) have no frame.
        let r = app.popup_rect.get().unwrap_or_else(|| {
            let a = app.pane_area();
            SRect {
                x: a.x,
                y: a.y,
                w: a.w,
                h: a.h,
            }
        });
        let inside =
            me.column >= r.x && me.column < r.x + r.w && me.row >= r.y && me.row < r.y + r.h;
        if !inside {
            app.on_key(KeyEvent::new(
                Key::Named(NamedKey::Escape),
                vk_proto::input::Mods::empty(),
            ));
            app.dirty = true;
        }
    }
    true
}

/// A pane runs something other than a shell in the foreground.
pub fn pane_running(app: &App, mi: usize, pane: &str) -> bool {
    let shell = |s: &str| {
        matches!(
            s.rsplit('/').next().unwrap_or(s).trim_start_matches('-'),
            "zsh" | "bash" | "fish" | "sh" | "dash" | "nu" | "tcsh" | "ksh"
        )
    };
    app.machines.get(mi).is_some_and(|m| {
        m.model
            .panes
            .iter()
            .find(|p| p.id == pane)
            .is_some_and(|p| !p.fg_cmdline.is_empty() && !shell(&p.fg_cmdline[0]))
    })
}

/// Some pane of workspace `ws` on machine `mi` runs something other than a shell.
pub fn workspace_busy(app: &App, mi: usize, ws: &str) -> bool {
    let Some(m) = app.machines.get(mi) else {
        return false;
    };
    m.model
        .panes
        .iter()
        .filter(|p| p.workspace == ws)
        .any(|p| pane_running(app, mi, &p.id))
}

/// `ui.confirm_close`: `running` asks when something other than a shell runs in what closes,
/// `always` always asks, `never` never does. The same rule for panes, tabs and workspaces.
pub fn should_confirm_close(app: &App, busy: bool) -> bool {
    match app.config.ui.confirm_close {
        vk_config::ConfirmClose::Always => true,
        vk_config::ConfirmClose::Never => false,
        vk_config::ConfirmClose::Running => busy,
    }
}

/// Close a pane, tab or workspace (`what`: "pane", "tab 2", "workspace api"), asking first
/// when [`should_confirm_close`] says so.
pub fn close_or_confirm(app: &mut App, action: crate::app::Action, busy: bool, what: &str) {
    if should_confirm_close(app, busy) {
        let message = if busy {
            format!("Close {what}? A process is running.")
        } else {
            format!("Close {what}?")
        };
        app.mode = Mode::Popup(Popup::Confirm {
            message,
            action: Box::new(action),
        });
    } else {
        app.confirm(action);
    }
}

/// Open interactions on agents the user isn't looking at, oldest first (08 §6.6).
pub fn inbox(app: &App) -> Vec<(usize, Interaction)> {
    let focused = app.focused_pane();
    let mut v: Vec<(usize, Interaction)> = Vec::new();
    for (mi, m) in app.machines.iter().enumerate() {
        for i in &m.model.interactions {
            if i.status == InteractionStatus::Open
                && !(mi == app.cur && Some(&i.pane) == focused.as_ref())
            {
                v.push((mi, i.clone()));
            }
        }
    }
    v.sort_by_key(|(_, i)| i.opened_at_ms);
    v
}

fn find_interaction(app: &App, id: &str) -> Option<(usize, Interaction)> {
    app.machines.iter().enumerate().find_map(|(mi, m)| {
        m.model
            .interactions
            .iter()
            .find(|i| i.id == id)
            .map(|i| (mi, i.clone()))
    })
}

pub fn key(app: &mut App, ev: KeyEvent, p: Popup) {
    let esc = matches!(ev.key, Key::Named(NamedKey::Escape));
    // A release is not a key press: the release of the key that opened one of these must not
    // close it, grant a clipboard write or focus a pane.
    if ev.kind == KeyKind::Release
        && matches!(
            p,
            Popup::Message { .. } | Popup::ClipboardAsk { .. } | Popup::Peek { .. }
        )
    {
        app.mode = Mode::Popup(p);
        return;
    }
    match p {
        Popup::Message { .. } => {}
        // Only `y` confirms (never enter: the release of the enter that opened it, or an enter
        // typed for the pane, must not close anything), and only through the decision guard.
        Popup::Confirm { action, message } => {
            let decides = matches!(ev.key, Key::Char('y' | 'Y'));
            let target = crate::decide::confirm_target(&message);
            let gate = app
                .decide
                .gate(Some(&target), &ev, Instant::now(), app.kitty, decides);
            match (gate, ev.key) {
                (crate::decide::Gate::Go, Key::Char('y' | 'Y')) => app.confirm(*action),
                (crate::decide::Gate::Go, Key::Char('n' | 'N') | Key::Named(NamedKey::Escape)) => {}
                _ => app.mode = Mode::Popup(Popup::Confirm { action, message }),
            }
        }
        // Opened deliberately via `review_clipboard`; only explicit keys act, others are ignored.
        Popup::ClipboardAsk {
            machine,
            data,
            primary,
        } => match ev.key {
            Key::Char('y' | 'Y') => {
                app.machines[machine].clipboard_allowed = Some(true);
                app.set_clipboard(&data, primary);
            }
            Key::Char('o' | 'O') => app.set_clipboard(&data, primary),
            Key::Char('n' | 'N') => {
                app.machines[machine].clipboard_allowed = Some(false);
                app.toast(format!(
                    "clipboard writes from {} denied",
                    app.machines[machine].label
                ));
            }
            // Dismiss without a decision; the machine can ask again.
            Key::Named(NamedKey::Escape) => {}
            _ => {
                app.mode = Mode::Popup(Popup::ClipboardAsk {
                    machine,
                    data,
                    primary,
                })
            }
        },
        p @ Popup::PasteAsk { .. } => crate::upload::ask_key(app, ev, p),
        Popup::BrowserDrop(a) => crate::browser_io::drop_key(app, ev, a),
        p @ (Popup::GroupPick { .. } | Popup::Search(_) | Popup::LayoutPick { .. }) => {
            crate::parity::popup_key(app, ev, p)
        }
        Popup::Goto { filter, sel } => crate::nav::goto_key(app, ev, filter, sel),
        Popup::Palette { filter, sel } => crate::nav::palette_key(app, ev, filter, sel),
        Popup::Hints(h) => crate::nav::hints_key(app, ev, h),
        Popup::PluginLink(c) => crate::plugins::link_key(app, ev, c),
        Popup::ClipboardRead(r) => crate::osc::read_key(app, ev, r),
        Popup::Inbox => crate::inbox::key(app, ev),
        Popup::Track => crate::tasks::track_key(app, ev),
        Popup::Task => crate::tasks::task_key(app, ev),
        Popup::PendingOps { sel, confirm } => crate::app::pending_ops_key(app, ev, sel, confirm),
        Popup::Previews => crate::preview_manager::key(app, ev),
        Popup::Gallery => crate::gallery::key(app, ev),
        Popup::ImageViewer => crate::viewer::key(app, ev),
        Popup::Desk => crate::desk::key(app, ev),
        Popup::Drafts => crate::drafts::key(app, ev),
        Popup::Assist => crate::assist::key(app, ev),
        Popup::Scrollback => crate::scrollback::key(app, ev),
        p @ (Popup::Onboarding | Popup::Batch | Popup::Fleet | Popup::TrustRepo) => {
            crate::ux::popup_key(app, ev, p)
        }
        Popup::Elevate => crate::elevate::key(app, ev),
        Popup::Collision => crate::collision::key(app, ev),
        Popup::Agents { filter, sel } => crate::agent_list::key(app, ev, filter, sel),
        Popup::Path(p) => crate::path_picker::popup_key(app, ev, p),
        Popup::HandoffAccept => crate::handoff::accept_key(app, ev),
        Popup::Handoffs => crate::connections::key(app, ev, crate::connections::Tab::Handoffs),
        Popup::HandoffSend => crate::handoff::send_key(app, ev),
        Popup::Sharing => crate::connections::key(app, ev, crate::connections::Tab::Hosts),
        Popup::Devices => crate::connections::key(app, ev, crate::connections::Tab::Devices),
        Popup::People => crate::connections::key(app, ev, crate::connections::Tab::People),
        Popup::CloudSend => crate::cloud::key(app, ev),
        Popup::Sandboxes => crate::sandboxes::key(app, ev),
        Popup::Updates => crate::updates::key(app, &ev),
        Popup::Peek { pane } => match ev.key {
            _ if esc => {}
            Key::Named(NamedKey::Enter) => {
                let mi = app.cur;
                app.return_to.clear();
                app.machines[mi].send(vk_proto::render::ClientFrame::Focus { pane });
            }
            // Track this work, or open the tracked task's details.
            Key::Char('t') => {
                let mi = app.cur;
                let run = app.machines[mi]
                    .model
                    .runs
                    .iter()
                    .find(|r| r.pane == pane)
                    .cloned();
                match run
                    .as_ref()
                    .and_then(|r| crate::tasks::task_for_run(app, mi, r))
                {
                    Some(t) => {
                        app.return_to.push(Popup::Peek { pane: pane.clone() });
                        crate::tasks::open_task(app, mi, &t);
                    }
                    None if run.is_some() => {
                        app.return_to.push(Popup::Peek { pane: pane.clone() });
                        crate::tasks::open_track(app, mi, &pane);
                    }
                    None => app.mode = Mode::Popup(Popup::Peek { pane }),
                }
            }
            Key::Char('a') => {
                let int = app
                    .m()
                    .model
                    .interactions
                    .iter()
                    .find(|i| i.pane == pane && i.status == InteractionStatus::Open)
                    .map(|i| i.id.clone());
                match int {
                    Some(i) => {
                        let mi = app.cur;
                        crate::popup_pane::open_card(app, mi, &i);
                    }
                    None => app.mode = Mode::Popup(Popup::Peek { pane }),
                }
            }
            // Watch the agent's browser session (06 B7).
            Key::Char('w') => {
                let mi = app.cur;
                app.return_to.clear();
                crate::nav::watch_session(app, mi, json!({"agent_pane": pane}));
            }
            Key::Char('r') | Key::Char('i') => {
                app.mode = Mode::Prompt(crate::app::Prompt {
                    kind: PromptKind::AgentReply { pane },
                    label: "reply (enter send · ctrl+d save as draft)".into(),
                    input: String::new(),
                });
            }
            // Drafts for this agent's workspace, its screenshots, a suggested title (08 §6.7,
            // 06 B8, 14).
            Key::Char('d') => crate::drafts::open_from_peek(app, &pane),
            Key::Char('p') => crate::gallery::open_from_peek(app, &pane),
            Key::Char('s') => {
                let mi = app.cur;
                crate::assist::suggest_title(
                    app,
                    mi,
                    &pane,
                    crate::assist::Origin::Peek(pane.clone()),
                );
            }
            _ => app.mode = Mode::Popup(Popup::Peek { pane }),
        },
        Popup::Card {
            interaction,
            mut sel,
        } => {
            // Releases, repeats and decision keys right after the card appeared (or changed
            // under the user) stop here (crate::decide).
            let decides = matches!(
                ev.key,
                Key::Char('y' | 's' | 'n' | 'c' | 'x' | ' ' | '1'..='9')
                    | Key::Named(NamedKey::Enter)
            );
            let target = crate::decide::card_target(&interaction);
            match app
                .decide
                .gate(Some(&target), &ev, Instant::now(), app.kitty, decides)
            {
                crate::decide::Gate::Go => {}
                crate::decide::Gate::Ignore | crate::decide::Gate::Wait => {
                    app.mode = Mode::Popup(Popup::Card { interaction, sel });
                    return;
                }
            }
            let Some((mi, it)) = find_interaction(app, &interaction) else {
                if !esc {
                    app.toast("that request is gone (answered, dismissed or withdrawn)");
                }
                return;
            };
            let open = it.status == InteractionStatus::Open && it.answerable;
            // `x` twice dismisses; any other key cancels the confirmation.
            let ikey = crate::inbox::ItemKey {
                machine: mi,
                kind: "interaction".into(),
                id: it.id.clone(),
            };
            let confirmed =
                app.inbox.confirm.take() == Some(crate::inbox::Confirm::Item(ikey.clone()));
            match (it.kind, ev.key) {
                (_, Key::Named(NamedKey::Escape)) => {}
                (_, Key::Char('x')) => {
                    if it.status != InteractionStatus::Open {
                        app.toast("this question is already closed");
                    } else if !app.machines[mi].capabilities().host {
                        app.toast("Dismiss is unavailable in a shared terminal");
                    } else if !confirmed {
                        app.inbox.confirm = Some(crate::inbox::Confirm::Item(ikey));
                        app.mode = Mode::Popup(Popup::Card { interaction, sel });
                    } else {
                        let title = interaction_title(&it);
                        crate::inbox::dismiss_interaction(app, mi, &it.id, &title);
                    }
                }
                (_, Key::Char('o')) => {
                    app.cur = mi;
                    app.return_to.clear();
                    app.machines[mi].send(vk_proto::render::ClientFrame::Focus {
                        pane: it.pane.clone(),
                    });
                }
                (InteractionKind::Approval | InteractionKind::PlanReview, Key::Char('y'))
                    if open =>
                {
                    app.answer(mi, &it.id, json!({"decision": "allow"}))
                }
                (InteractionKind::Approval, Key::Char('s')) if open => app.answer(
                    mi,
                    &it.id,
                    json!({"decision": "allow_always", "scope": "session"}),
                ),
                (InteractionKind::Approval | InteractionKind::PlanReview, Key::Char('n'))
                    if open =>
                {
                    app.answer(mi, &it.id, json!({"decision": "deny"}))
                }
                (InteractionKind::Approval | InteractionKind::PlanReview, Key::Char('e'))
                    if open =>
                {
                    app.mode = Mode::Prompt(crate::app::Prompt {
                        kind: PromptKind::CardText {
                            interaction: it.id.clone(),
                        },
                        label: "deny with message".into(),
                        input: String::new(),
                    });
                }
                (
                    InteractionKind::Question | InteractionKind::Picker,
                    Key::Char('j') | Key::Named(NamedKey::Down),
                ) => {
                    let n = it.questions.first().map(|q| q.options.len()).unwrap_or(0);
                    sel = (sel + 1).min(n.saturating_sub(1));
                    app.mode = Mode::Popup(Popup::Card { interaction, sel });
                }
                (
                    InteractionKind::Question | InteractionKind::Picker,
                    Key::Char('k') | Key::Named(NamedKey::Up),
                ) => {
                    sel = sel.saturating_sub(1);
                    app.mode = Mode::Popup(Popup::Card { interaction, sel });
                }
                (
                    InteractionKind::Question | InteractionKind::Picker,
                    Key::Named(NamedKey::Enter) | Key::Char(' '),
                ) if open => {
                    if let Some(a) = choice_answer(&it, sel) {
                        app.answer(mi, &it.id, a);
                    }
                }
                (InteractionKind::Question | InteractionKind::Picker, Key::Char(c))
                    if open && c.is_ascii_digit() =>
                {
                    let idx = c.to_digit(10).unwrap_or(1).saturating_sub(1) as usize;
                    if let Some(a) = choice_answer(&it, idx) {
                        app.answer(mi, &it.id, a);
                    }
                }
                (InteractionKind::Picker, Key::Char('c')) if open => {
                    app.answer(mi, &it.id, json!({"decision": "cancel"}))
                }
                // The batch view of equivalent approvals (08 §8).
                (_, Key::Char('A')) => crate::batch::open(app, Some((mi, it.id.clone()))),
                (_, Key::Char(']')) => {
                    let items = inbox(app);
                    let pos = items.iter().position(|(_, x)| x.id == it.id).unwrap_or(0);
                    if let Some((m2, n)) = items.get((pos + 1) % items.len().max(1)) {
                        app.cur = *m2;
                        app.mode = Mode::Popup(Popup::Card {
                            interaction: n.id.clone(),
                            sel: 0,
                        });
                    }
                }
                _ => app.mode = Mode::Popup(Popup::Card { interaction, sel }),
            }
        }
    }
    let _ = Pending::Ignore;
}

pub fn draw(app: &App, g: &mut Grid) -> Option<(u16, u16, CursorShape)> {
    let t = app.theme;
    app.popup_rect.set(None);
    match &app.mode {
        Mode::Prompt(p) => {
            let area = app.pane_area();
            let y = area.y + area.h.saturating_sub(1);
            g.fill(
                SRect {
                    x: area.x,
                    y,
                    w: area.w,
                    h: 1,
                },
                t.text(),
            );
            let s = format!(" {}: {}", p.label, p.input);
            let used = g.put_str(area.x, y, &s, t.bold(t.fg), area.w);
            return Some((
                area.x + used.min(area.w.saturating_sub(1)),
                y,
                CursorShape::Bar,
            ));
        }
        Mode::Popup(p) => match p {
            Popup::Message { title, body } => {
                let mut b = frame(app, g, 70, 12, title);
                for l in body.lines() {
                    b.line(l, t.text());
                }
            }
            Popup::Confirm { message, .. } => {
                let body = hard_wrap(message, 52);
                let mut b = frame(app, g, 56, body.len() as u16 + 4, "confirm");
                for l in &body {
                    b.line(l, t.text());
                }
                b.line("", t.text());
                b.line(
                    if crate::decide::live(app) {
                        "[y] yes   [n] no"
                    } else {
                        "[y] yes (in a moment)   [n] no"
                    },
                    t.dim(),
                );
            }
            Popup::PasteAsk {
                machine,
                pane,
                items,
                sel,
                ..
            } => {
                let h = (items.len().min(6) + 6) as u16;
                let mut b = frame(app, g, 72, h, "paste local files");
                let total: u64 = items.iter().map(|i| i.size).sum();
                b.line(
                    &format!(
                        "Upload {} file(s) ({}) to {} for pane {}?",
                        items.len(),
                        crate::upload::human(total),
                        app.machines[*machine].label,
                        pane
                    ),
                    t.text(),
                );
                for it in items.iter().take(6) {
                    b.line(
                        &format!("  {}  {}", it.name, crate::upload::human(it.size)),
                        t.dim(),
                    );
                }
                if items.len() > 6 {
                    b.line(&format!("  … and {} more", items.len() - 6), t.dim());
                }
                let btn = |i: usize, s: &str| {
                    if i == *sel {
                        format!("[> {s} <]")
                    } else {
                        format!("[ {s} ]")
                    }
                };
                b.line(
                    &format!(
                        "{} {} {}",
                        btn(0, "u upload+paste paths"),
                        btn(1, "o paste original"),
                        btn(2, "esc cancel")
                    ),
                    t.text(),
                );
                b.line(
                    "tab/arrows select, enter confirm; other keys are ignored",
                    t.dim(),
                );
            }
            Popup::ClipboardAsk { machine, data, .. } => {
                let mut b = frame(app, g, 66, 7, "clipboard");
                b.line(
                    &format!(
                        "{} wants to set your clipboard ({} bytes).",
                        app.machines[*machine].label,
                        data.len()
                    ),
                    t.text(),
                );
                b.line("[y] allow for this machine   [o] allow once", t.dim());
                b.line("[n] deny for this machine   [esc] dismiss", t.dim());
                if app.clip.dropped > 0 {
                    b.line(
                        &format!("{} further write(s) dropped (size/rate)", app.clip.dropped),
                        t.dim(),
                    );
                }
            }
            Popup::Goto { filter, sel } => {
                let (x, y) = crate::nav::draw_goto(app, g, filter, *sel);
                return Some((x, y, CursorShape::Bar));
            }
            Popup::Palette { filter, sel } => {
                let (x, y) = crate::nav::draw_palette(app, g, filter, *sel);
                return Some((x, y, CursorShape::Bar));
            }
            Popup::Hints(h) => crate::nav::draw_hints(app, g, h),
            Popup::PluginLink(c) => crate::plugins::draw_link(app, g, c),
            Popup::ClipboardRead(r) => crate::osc::draw_read(app, g, r),
            p @ (Popup::GroupPick { .. } | Popup::Search(_) | Popup::LayoutPick { .. }) => {
                return crate::parity::popup_draw(app, g, p);
            }
            Popup::Inbox => crate::inbox::draw(app, g),
            Popup::Task => crate::tasks::draw_task(app, g),
            Popup::Track => crate::tasks::draw_track(app, g),
            Popup::PendingOps { sel, confirm } => {
                crate::app::draw_pending_ops(app, g, *sel, *confirm)
            }
            Popup::Previews => crate::preview_manager::draw(app, g),
            Popup::Gallery => crate::gallery::draw(app, g),
            Popup::ImageViewer => crate::viewer::draw(app, g),
            Popup::Desk => crate::desk::draw(app, g),
            Popup::Drafts => crate::drafts::draw(app, g),
            Popup::Assist => crate::assist::draw(app, g),
            Popup::Scrollback => crate::scrollback::draw(app, g),
            p @ (Popup::Onboarding | Popup::Batch | Popup::Fleet | Popup::TrustRepo) => {
                crate::ux::popup_draw(app, g, p)
            }
            Popup::Elevate => crate::elevate::draw(app, g),
            Popup::Collision => crate::collision::draw(app, g),
            Popup::Agents { filter, sel } => {
                let (x, y) = crate::agent_list::draw(app, g, filter, *sel);
                return Some((x, y, CursorShape::Bar));
            }
            Popup::Path(p) => {
                let (x, y) = crate::path_picker::popup_draw(app, g, p);
                return Some((x, y, CursorShape::Bar));
            }
            Popup::HandoffAccept => {
                let (x, y) = crate::handoff::draw_accept(app, g)?;
                return Some((x, y, CursorShape::Bar));
            }
            Popup::Handoffs => crate::handoff::draw_list(app, g),
            Popup::HandoffSend => {
                let (x, y) = crate::handoff::draw_send(app, g)?;
                return Some((x, y, CursorShape::Bar));
            }
            Popup::Sharing => {
                let (x, y) = crate::sharing::draw(app, g)?;
                return Some((x, y, CursorShape::Bar));
            }
            Popup::Devices => crate::devices::draw(app, g),
            Popup::People => {
                let (x, y) = crate::people::draw(app, g)?;
                return Some((x, y, CursorShape::Bar));
            }
            Popup::CloudSend => {
                let (x, y) = crate::cloud::draw(app, g)?;
                return Some((x, y, CursorShape::Bar));
            }
            Popup::Sandboxes => {
                let (x, y) = crate::sandboxes::draw(app, g)?;
                return Some((x, y, CursorShape::Bar));
            }
            Popup::Updates => crate::updates::draw(app, g),
            Popup::BrowserDrop(a) => crate::browser_io::draw_drop(app, g, a),
            Popup::Peek { pane } => {
                let m = app.m();
                let run = m.model.runs.iter().find(|r| &r.pane == pane);
                let title = match run {
                    Some(r) => format!(
                        "{} {} · {}",
                        harness_icon(&r.harness),
                        r.label(),
                        r.execution.value.as_str()
                    ),
                    None => "peek".into(),
                };
                let mut b = frame(app, g, 90, 26, &title);
                if let Some(r) = run {
                    let (glyph, label, color, inferred) = run_state(app, m, r);
                    b.line(
                        &format!(
                            "{glyph} {label}{}",
                            if inferred {
                                "  (inferred from screen)"
                            } else {
                                ""
                            }
                        ),
                        t.s(color),
                    );
                    if let Some(msg) = &r.last_message {
                        for l in msg.lines().take(8) {
                            b.line(l, t.text());
                        }
                    }
                    if let Some(tool) = &r.last_tool {
                        b.line(&format!("last tool: {tool}"), t.dim());
                    }
                    for bg in r.bg_tasks.iter().take(6) {
                        b.line(
                            &format!(
                                "⧗ {} · {} · {}",
                                bg.kind,
                                crate::draw::truncate(&bg.label, 60),
                                crate::draw::bg_age(bg.since_ms)
                            ),
                            t.dim(),
                        );
                    }
                    if let Some(v) = crate::plugins::agent_view(app, app.cur, &r.id) {
                        let c = match v.tone.as_str() {
                            "ok" => t.green,
                            "warn" => t.yellow,
                            "error" => t.red,
                            _ => t.accent,
                        };
                        b.line(&format!("▸ {} ({})", v.text, v.plugin), t.s(c));
                        if let Some(d) = &v.detail {
                            for l in d.lines().take(6) {
                                b.line(l, t.dim());
                            }
                        }
                    }
                }
                if let Some(buf) = m.panes.get(pane) {
                    b.line("─── screen ───", t.dim());
                    let lines: Vec<String> = buf
                        .lines
                        .iter()
                        .map(|l| l.text().trim_end().to_string())
                        .collect();
                    let last = lines
                        .iter()
                        .rposition(|l| !l.is_empty())
                        .map(|i| i + 1)
                        .unwrap_or(0);
                    for l in &lines[last.saturating_sub(14)..last] {
                        b.line(l, t.text());
                    }
                }
                let tracked = run.and_then(|r| crate::tasks::task_for_run(app, app.cur, r));
                let browsing = crate::nav::session_of_pane(app, app.cur, pane);
                if let Some(s) = browsing {
                    b.line(
                        &format!(
                            "◉ browsing {} · {}{}  [w] watch",
                            s.handle,
                            s.url
                                .trim_start_matches("http://")
                                .trim_start_matches("https://"),
                            if s.human_control {
                                " · taken over"
                            } else {
                                ""
                            }
                        ),
                        t.s(t.accent),
                    );
                }
                if let Some(n) = crate::gallery::badge(app, app.cur, pane) {
                    b.line(
                        &format!("📷 {n} new screenshot(s)  [p] screenshots"),
                        t.s(t.accent),
                    );
                }
                b.line(
                    if tracked.is_some() {
                        "[enter] focus  [a] answer  [r] reply  [t] task details  [w] watch browser  [esc] close"
                    } else {
                        "[enter] focus  [a] answer  [r] reply  [t] track this work  [w] watch browser  [esc] close"
                    },
                    t.dim(),
                );
                b.line(
                    "[d] drafts  [p] screenshots  [s] suggest title (assistant)",
                    t.dim(),
                );
            }
            Popup::Card { interaction, sel } => {
                let (mi, it) = find_interaction(app, interaction)?;
                draw_card(app, g, mi, &it, *sel);
            }
        },
        Mode::Prefix(p) if p.menu => crate::menu::draw(app, g),
        Mode::Resize => crate::menu::draw_resize(app, g),
        _ => {}
    }
    None
}

/// A short name for an interaction in toasts: its handle and what it asks.
pub fn interaction_title(it: &Interaction) -> String {
    let what = it
        .action
        .as_ref()
        .and_then(|a| a.command.as_deref().or(Some(a.summary.as_str())))
        .filter(|s| !s.is_empty())
        .unwrap_or(&it.title);
    let what = what.lines().next().unwrap_or_default();
    if it.handle.is_empty() {
        what.to_string()
    } else {
        format!("{} {what}", it.handle)
    }
}

/// The `interaction.answer` params for picking row `idx`: one option, or for a multi-select
/// picker the full desired checked set (the current set with that row toggled).
pub fn choice_answer(it: &Interaction, idx: usize) -> Option<serde_json::Value> {
    let q = it.questions.first()?;
    let o = q.options.get(idx)?;
    let ids: Vec<String> = if it.kind == InteractionKind::Picker && q.multi {
        q.options
            .iter()
            .filter(|x| x.selected != (x.id == o.id))
            .map(|x| x.id.clone())
            .collect()
    } else {
        vec![o.id.clone()]
    };
    let mut a = json!({"choices": {q.id.clone(): ids}});
    if let Some(p) = &it.picker {
        a["expected_signature"] = json!(p.signature);
    }
    Some(a)
}

/// The card: what is asked, wrapped to the frame, then its keys. The key hint is always on
/// screen; a body taller than the frame ends with how much is not shown and that `[o]` opens
/// the pane with the full request.
fn draw_card(app: &App, g: &mut Grid, mi: usize, it: &Interaction, sel: usize) {
    let t = app.theme;
    let m = &app.machines[mi];
    let run = m.model.runs.iter().find(|r| r.id == it.run);
    let ws = m
        .model
        .panes
        .iter()
        .find(|p| p.id == it.pane)
        .and_then(|p| m.model.workspaces.iter().find(|w| w.id == p.workspace))
        .map(|w| w.display_name().to_string())
        .unwrap_or_default();
    let who = run.map(|r| r.label().to_string()).unwrap_or_default();
    let kind = match it.kind {
        InteractionKind::Approval => "needs approval",
        InteractionKind::Question => "question",
        InteractionKind::PlanReview => "plan review",
        InteractionKind::Notice => "notice",
        InteractionKind::Picker => "picker",
    };
    let area = app.pane_area();
    let w = 80u16.min(area.w.saturating_sub(2)).max(20);
    let inner = w.saturating_sub(4).max(1) as usize;
    let mut body = Vec::new();
    card_lines(app, mi, it, sel, inner, &mut body);
    let keys = if !it.answerable {
        "this dialog can only be answered in the pane · [o] open pane  [esc] later".to_string()
    } else {
        match it.kind {
            InteractionKind::Approval => "[y] allow once  [s] allow for session  [n] deny  [e] deny with message  [A] batch  [x] dismiss  [o] open pane  [esc] later".into(),
            InteractionKind::PlanReview => "[y] approve  [e] request changes  [n] reject  [x] dismiss  [o] open pane  [esc] later".into(),
            InteractionKind::Question => "[j/k] choose  [enter] answer  [1-9] pick  [x] dismiss  [o] open pane  [esc] later".into(),
            InteractionKind::Notice => "[o] open pane  [esc] close".into(),
            InteractionKind::Picker if it.questions.iter().all(|q| q.options.is_empty()) => "[c] cancel the dialog  [o] open pane  [esc] later".into(),
            InteractionKind::Picker if it.questions.first().is_some_and(|q| q.multi) => "[j/k] choose  [enter] toggle and confirm  [c] cancel  [o] open pane  [esc] later".into(),
            InteractionKind::Picker => "[j/k] choose  [enter] pick  [c] cancel  [o] open pane  [esc] later".into(),
        }
    };
    let mut foot: Vec<(String, Style)> = vec![(String::new(), t.text())];
    let confirming = app.inbox.confirm
        == Some(crate::inbox::Confirm::Item(crate::inbox::ItemKey {
            machine: mi,
            kind: "interaction".into(),
            id: it.id.clone(),
        }));
    if confirming {
        for l in word_wrap(
            "The agent may still be waiting in the pane. Press x again to dismiss.",
            inner,
        ) {
            foot.push((l, t.s(t.yellow)));
        }
    }
    if it.answerable && !crate::decide::live(app) {
        foot.push((crate::decide::WAIT_HINT.into(), t.s(t.yellow)));
    }
    for l in word_wrap(&keys, inner) {
        foot.push((l, t.dim()));
    }
    // Border (2) + body + footer, within the pane area.
    let want = (body.len() + foot.len() + 2) as u16;
    let mut b = frame(
        app,
        g,
        w,
        want.max(8),
        &format!("{who} · {ws} · {kind} · {}", it.handle),
    );
    let rows = b.rows_left() as usize;
    let room = rows.saturating_sub(foot.len());
    if body.len() > room {
        let keep = room.saturating_sub(1);
        let hidden = body.len() - keep;
        body.truncate(keep);
        body.push((
            format!("… {hidden} more line(s) · [o] open the pane to see the full request"),
            t.s(t.yellow),
        ));
    }
    // A frame too small for the whole footer keeps its last lines: the keys.
    let foot_skip = foot
        .len()
        .saturating_sub(rows.saturating_sub(body.len().min(rows)));
    for (s, st) in body.into_iter().take(rows) {
        b.line(&s, st);
    }
    for (s, st) in foot.into_iter().skip(foot_skip) {
        b.line(&s, st);
    }
}

/// Split `s` into rows of at most `w` columns, keeping every character (commands and paths
/// must read exactly as the agent sent them, spaces included).
pub fn hard_wrap(s: &str, w: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthChar;
    let w = w.max(1);
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut used = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if used + cw > w && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            used = 0;
        }
        cur.push(c);
        used += cw;
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    out
}

/// Wrap prose at spaces (a word longer than `w` is split).
pub fn word_wrap(s: &str, w: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthStr;
    let w = w.max(1);
    let mut out = Vec::new();
    let mut cur = String::new();
    for word in s.split(' ') {
        let need = if cur.is_empty() {
            word.width()
        } else {
            cur.width() + 1 + word.width()
        };
        if need > w && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
        while cur.width() > w {
            let mut parts = hard_wrap(&cur, w);
            cur = parts.pop().unwrap_or_default();
            out.extend(parts);
        }
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    out
}

/// At most this many rows of a command, a plan or a diff before "… N more".
const CMD_ROWS: usize = 12;
const PATHS: usize = 6;
const DIFF_ROWS: usize = 12;
const PLAN_ROWS: usize = 12;

/// The body of an interaction card (shared by the card popup and the inbox detail), wrapped to
/// `width` columns. Long parts end with how much is left out.
pub fn card_lines(
    app: &App,
    mi: usize,
    it: &Interaction,
    sel: usize,
    width: usize,
    out: &mut Vec<(String, Style)>,
) {
    let t = app.theme;
    let width = width.max(8);
    let mut b = Lines(out);
    if let Some(a) = &it.action {
        let risk = match a.risk {
            Risk::High => ("high", t.red),
            Risk::Medium => ("medium", t.yellow),
            Risk::Low => ("low", t.green),
            Risk::Unknown => ("unknown", t.muted),
        };
        b.line(&format!("{}   risk: {}", a.tool, risk.0), t.bold(risk.1));
        if let Some(c) = &a.command {
            push_block(&mut b, &t, width, c, "  ", CMD_ROWS, "line(s)", &|_| {
                t.text()
            });
        }
        for p in a.paths.iter().take(PATHS) {
            for r in hard_wrap(p, width.saturating_sub(2)) {
                b.line(&format!("  {r}"), t.text());
            }
        }
        if a.paths.len() > PATHS {
            b.line(
                &format!("  … {} more path(s)", a.paths.len() - PATHS),
                t.s(t.yellow),
            );
        }
        if !a.risk_reasons.is_empty() {
            for l in word_wrap(&format!("reasons: {}", a.risk_reasons.join(", ")), width) {
                b.line(&l, t.dim());
            }
        }
        if let Some(d) = &a.diff {
            let color = |l: &str| {
                t.s(if l.starts_with('+') {
                    t.green
                } else if l.starts_with('-') {
                    t.red
                } else {
                    t.muted
                })
            };
            push_block(&mut b, &t, width, d, "", DIFF_ROWS, "diff line(s)", &color);
        }
    } else {
        for l in word_wrap(&it.title, width) {
            b.line(&l, t.bold(t.fg));
        }
    }
    if let Some(plan) = &it.plan_md {
        push_block(
            &mut b,
            &t,
            width,
            plan,
            "",
            PLAN_ROWS,
            "plan line(s)",
            &|_| t.text(),
        );
    }
    for q in &it.questions {
        for l in word_wrap(&q.prompt, width) {
            b.line(&l, t.bold(t.fg));
        }
        for (i, o) in q.options.iter().enumerate() {
            let st = if i == sel { t.sel(t.accent) } else { t.text() };
            let d = o
                .description
                .as_ref()
                .map(|d| format!(" — {d}"))
                .unwrap_or_default();
            // Pickers mark the harness's current row (or checked boxes in a multi-select).
            let mark = match (it.kind, q.multi, o.selected) {
                (InteractionKind::Picker, true, true) => "[x] ",
                (InteractionKind::Picker, true, false) => "[ ] ",
                (InteractionKind::Picker, false, true) => "● ",
                (InteractionKind::Picker, false, false) => "  ",
                _ => "",
            };
            let rows = word_wrap(&format!("{}. {mark}{}{d}", i + 1, o.label), width);
            for (k, l) in rows.into_iter().enumerate() {
                let l = if k == 0 { l } else { format!("   {l}") };
                b.line(&l, st);
            }
        }
    }
    if let Some(lr) = it.picker.as_ref().and_then(|p| p.left_right.as_ref()) {
        b.line(
            &format!(
                "{}: {} (current: {})",
                lr.verb,
                lr.values.join(" · "),
                lr.current.as_deref().unwrap_or("?")
            ),
            t.dim(),
        );
    }
    if it.source == StateSource::Screen {
        b.line(
            &format!("inferred from screen ({:.2})", it.confidence),
            t.dim(),
        );
    }
    if let Some(t) = crate::gateway::interaction_answered_text(it) {
        b.line(&format!("📱 {t}"), app.theme.s(app.theme.accent));
    } else if let Some(how) = crate::closed::text(app, mi, it) {
        // How it closed: never "answered" for one that was dismissed or closed automatically.
        b.line(&how, t.dim());
    }
    match it.delivery {
        DeliveryState::DecisionRecorded | DeliveryState::Delivering => {
            b.line("delivering…", t.s(t.yellow))
        }
        DeliveryState::Delivered => b.line("✓ delivered", t.s(t.green)),
        DeliveryState::DeliveryUnknown => {
            b.line("couldn't confirm delivery — check the pane", t.s(t.red))
        }
        DeliveryState::Failed => b.line(
            &format!(
                "✗ delivery failed: {}",
                it.delivery_error.clone().unwrap_or_default()
            ),
            t.s(t.red),
        ),
        _ => {}
    }
}

/// Each line of `text` wrapped to `width` (indented by `indent`), at most `max` rows; a longer
/// text ends with how many of its lines are not shown.
#[allow(clippy::too_many_arguments)]
fn push_block(
    b: &mut Lines<'_>,
    t: &crate::theme::Theme,
    width: usize,
    text: &str,
    indent: &str,
    max: usize,
    what: &str,
    st: &dyn Fn(&str) -> Style,
) {
    let w = width.saturating_sub(indent.len()).max(1);
    let wrapped: Vec<(&str, Vec<String>)> = text.lines().map(|l| (l, hard_wrap(l, w))).collect();
    let total: usize = wrapped.iter().map(|(_, r)| r.len()).sum();
    if total <= max {
        for (l, rows) in &wrapped {
            for r in rows {
                b.line(&format!("{indent}{r}"), st(l));
            }
        }
        return;
    }
    // Leave a row for the "… N more" line; count what is left out in source lines.
    let room = max.saturating_sub(1);
    let mut used = 0;
    let mut whole = 0;
    'out: for (l, rows) in &wrapped {
        for r in rows {
            if used == room {
                break 'out;
            }
            b.line(&format!("{indent}{r}"), st(l));
            used += 1;
        }
        whole += 1;
    }
    b.line(
        &format!(
            "{indent}… {} more {what} · [o] open the pane to see all",
            wrapped.len() - whole
        ),
        t.s(t.yellow),
    );
}

struct Lines<'a>(&'a mut Vec<(String, Style)>);

impl Lines<'_> {
    fn line(&mut self, s: &str, st: Style) {
        self.0.push((s.to_string(), st));
    }
}
