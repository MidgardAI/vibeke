//! User-invoked popups (08 §6, §8): never opened spontaneously over the focused pane.

use crate::app::{App, Mode, Pending, Popup, PromptKind};
use crate::draw::{harness_icon, run_state};
use crate::screen::{Grid, Rect as SRect};
use serde_json::json;
use vk_proto::input::{Key, KeyEvent, NamedKey};
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

#[derive(Debug, Clone, PartialEq)]
pub enum GotoTarget {
    Pane(String),
    Task(String),
}

/// Goto entries: (label, machine, target). Panes/agents first, then tracked tasks (`#task`).
fn goto_entries(app: &App, filter: &str) -> Vec<(String, usize, GotoTarget)> {
    let f = filter.to_lowercase();
    let mut out = Vec::new();
    for (mi, m) in app.machines.iter().enumerate() {
        let mprefix = if app.machines.len() > 1 {
            format!("{}/", m.label)
        } else {
            String::new()
        };
        for w in &m.model.workspaces {
            for t in m.model.tabs.iter().filter(|t| t.workspace == w.id) {
                for pid in t.layout.panes() {
                    let Some(p) = m.model.panes.iter().find(|x| x.id == pid) else {
                        continue;
                    };
                    let run = m.model.runs.iter().find(|r| r.pane == pid);
                    let agent = run
                        .map(|r| {
                            format!(
                                " @{} {}",
                                r.name.clone().unwrap_or_else(|| r.harness.clone()),
                                r.execution.value.as_str()
                            )
                        })
                        .unwrap_or_default();
                    let label = format!(
                        "{mprefix}{} :{} {} {}{agent}",
                        w.display_name(),
                        t.number,
                        t.title.clone().unwrap_or_default(),
                        p.display_title()
                    );
                    let hay = label.to_lowercase();
                    let ok = f.split_whitespace().all(|tok| {
                        if let Some(state) = tok.strip_prefix('!') {
                            run.is_some_and(|r| r.execution.value.as_str().starts_with(state))
                                || (state.starts_with("appr")
                                    && m.model.interactions.iter().any(|i| {
                                        i.pane == pid && i.status == InteractionStatus::Open
                                    }))
                        } else {
                            hay.contains(tok.trim_start_matches(['@', '#', ':']))
                        }
                    });
                    if ok {
                        out.push((label, mi, GotoTarget::Pane(pid)));
                    }
                }
            }
        }
    }
    let wants_task = f.split_whitespace().any(|t| t.starts_with('#'));
    for (mi, m) in app.machines.iter().enumerate() {
        let mprefix = if app.machines.len() > 1 {
            format!("{}/", m.label)
        } else {
            String::new()
        };
        for t in m
            .model
            .tasks
            .iter()
            .filter(|t| t.status != "archived" && (t.intent_revision.is_some() || wants_task))
        {
            let label = format!(
                "{mprefix}#{} {} · {}",
                t.handle,
                t.title,
                t.review_label
                    .as_deref()
                    .map(crate::tasks::label_text)
                    .unwrap_or("task")
            );
            let hay = label.to_lowercase();
            if f.split_whitespace().all(|tok| {
                !tok.starts_with('!') && hay.contains(tok.trim_start_matches(['@', ':']))
            }) {
                out.push((label, mi, GotoTarget::Task(t.id.clone())));
            }
        }
    }
    out
}

pub fn key(app: &mut App, ev: KeyEvent, p: Popup) {
    let esc = matches!(ev.key, Key::Named(NamedKey::Escape));
    match p {
        Popup::Help | Popup::Message { .. } => {}
        Popup::Confirm { action, message } => match ev.key {
            Key::Char('y' | 'Y') | Key::Named(NamedKey::Enter) => app.confirm(*action),
            Key::Char('n' | 'N') | Key::Named(NamedKey::Escape) => {}
            _ => app.mode = Mode::Popup(Popup::Confirm { action, message }),
        },
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
        Popup::Goto {
            mut filter,
            mut sel,
        } => {
            let entries = goto_entries(app, &filter);
            match ev.key {
                _ if esc => {}
                Key::Named(NamedKey::Enter) => match entries.get(sel).cloned() {
                    Some((_, mi, GotoTarget::Pane(pane))) => {
                        app.cur = mi;
                        app.machines[mi].send(vk_proto::render::ClientFrame::Focus { pane });
                    }
                    Some((_, mi, GotoTarget::Task(t))) => crate::tasks::open_task(app, mi, &t),
                    None => {}
                },
                Key::Named(NamedKey::Down) | Key::Char('n')
                    if ev.mods.ctrl() || ev.key == Key::Named(NamedKey::Down) =>
                {
                    sel = (sel + 1).min(entries.len().saturating_sub(1));
                    app.mode = Mode::Popup(Popup::Goto { filter, sel });
                }
                Key::Named(NamedKey::Up) | Key::Char('p')
                    if ev.mods.ctrl() || ev.key == Key::Named(NamedKey::Up) =>
                {
                    sel = sel.saturating_sub(1);
                    app.mode = Mode::Popup(Popup::Goto { filter, sel });
                }
                Key::Named(NamedKey::Backspace) => {
                    filter.pop();
                    app.mode = Mode::Popup(Popup::Goto { filter, sel: 0 });
                }
                Key::Char(c) if !ev.mods.ctrl() => {
                    filter.push(c);
                    app.mode = Mode::Popup(Popup::Goto { filter, sel: 0 });
                }
                _ => app.mode = Mode::Popup(Popup::Goto { filter, sel }),
            }
        }
        Popup::Inbox => crate::inbox::key(app, ev),
        Popup::Track => crate::tasks::track_key(app, ev),
        Popup::Task => crate::tasks::task_key(app, ev),
        Popup::PendingOps { sel, confirm } => crate::app::pending_ops_key(app, ev, sel, confirm),
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
                        app.mode = Mode::Popup(Popup::Card {
                            interaction: i,
                            sel: 0,
                        })
                    }
                    None => app.mode = Mode::Popup(Popup::Peek { pane }),
                }
            }
            Key::Char('r') | Key::Char('i') => {
                app.mode = Mode::Prompt(crate::app::Prompt {
                    kind: PromptKind::AgentReply { pane },
                    label: "reply".into(),
                    input: String::new(),
                });
            }
            _ => app.mode = Mode::Popup(Popup::Peek { pane }),
        },
        Popup::Card {
            interaction,
            mut sel,
        } => {
            let Some((mi, it)) = find_interaction(app, &interaction) else {
                return;
            };
            let open = it.status == InteractionStatus::Open && it.answerable;
            match (it.kind, ev.key) {
                (_, Key::Named(NamedKey::Escape)) => {}
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
                (InteractionKind::Question, Key::Char('j') | Key::Named(NamedKey::Down)) => {
                    let n = it.questions.first().map(|q| q.options.len()).unwrap_or(0);
                    sel = (sel + 1).min(n.saturating_sub(1));
                    app.mode = Mode::Popup(Popup::Card { interaction, sel });
                }
                (InteractionKind::Question, Key::Char('k') | Key::Named(NamedKey::Up)) => {
                    sel = sel.saturating_sub(1);
                    app.mode = Mode::Popup(Popup::Card { interaction, sel });
                }
                (InteractionKind::Question, Key::Named(NamedKey::Enter) | Key::Char(' '))
                    if open =>
                {
                    if let Some(q) = it.questions.first()
                        && let Some(o) = q.options.get(sel)
                    {
                        app.answer(
                            mi,
                            &it.id,
                            json!({"choices": {q.id.clone(): [o.id.clone()]}}),
                        );
                    }
                }
                (InteractionKind::Question, Key::Char(c)) if open && c.is_ascii_digit() => {
                    let idx = c.to_digit(10).unwrap_or(1).saturating_sub(1) as usize;
                    if let Some(q) = it.questions.first()
                        && let Some(o) = q.options.get(idx)
                    {
                        app.answer(
                            mi,
                            &it.id,
                            json!({"choices": {q.id.clone(): [o.id.clone()]}}),
                        );
                    }
                }
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
            Popup::Help => {
                let mut b = frame(app, g, 72, 30, "help · esc to close");
                let km = &app.keymap;
                for (action, label) in [
                    ("split_vertical", "split side by side"),
                    ("split_horizontal", "split stacked"),
                    ("close_pane", "close pane"),
                    ("zoom", "zoom pane"),
                    ("focus_pane_left", "focus left (h j k l)"),
                    ("new_tab", "new tab"),
                    ("next_tab", "next tab (p previous, 1..9 jump)"),
                    ("rename_tab", "rename tab"),
                    ("new_workspace", "new workspace"),
                    (
                        "workspace_picker",
                        "navigate sidebar (space peek, a answer)",
                    ),
                    ("goto", "goto anything"),
                    ("next_attention", "next agent that needs you"),
                    ("enter_copy_mode", "copy mode (/ search, v select, y yank)"),
                    ("resize_mode", "resize mode"),
                    ("toggle_sidebar", "toggle sidebar"),
                    ("mark_unread", "mark unread"),
                    ("new_task", "new task (git worktree)"),
                    ("reload_config", "reload config"),
                    ("detach", "detach"),
                ] {
                    let k = km.binding_for(action).unwrap_or_else(|| "—".into());
                    b.line(&format!("{k:<18} {label}"), t.text());
                }
                b.line(
                    "prefix+i           inbox (f 5-minute view, s snooze, e effort)",
                    t.text(),
                );
                b.line(
                    "peek t             track this work / task details",
                    t.text(),
                );
                b.line(":track_work :task_details :pending_operations", t.text());
            }
            Popup::Message { title, body } => {
                let mut b = frame(app, g, 70, 12, title);
                for l in body.lines() {
                    b.line(l, t.text());
                }
            }
            Popup::Confirm { message, .. } => {
                let mut b = frame(app, g, 56, 5, "confirm");
                b.line(message, t.text());
                b.line("[y] yes   [n] no", t.dim());
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
                let entries = goto_entries(app, filter);
                let mut b = frame(
                    app,
                    g,
                    80,
                    22,
                    "goto · type to filter · !approve !working @name",
                );
                b.line(&format!("> {filter}"), t.bold(t.fg));
                for (i, (label, _, _)) in entries
                    .iter()
                    .enumerate()
                    .skip(sel.saturating_sub(15))
                    .take(18)
                {
                    let st = if i == *sel { t.sel(t.accent) } else { t.text() };
                    b.line(label, st);
                }
            }
            Popup::Inbox => crate::inbox::draw(app, g),
            Popup::Task => crate::tasks::draw_task(app, g),
            Popup::Track => crate::tasks::draw_track(app, g),
            Popup::PendingOps { sel, confirm } => {
                crate::app::draw_pending_ops(app, g, *sel, *confirm)
            }
            Popup::Peek { pane } => {
                let m = app.m();
                let run = m.model.runs.iter().find(|r| &r.pane == pane);
                let title = match run {
                    Some(r) => format!(
                        "{} {} · {}",
                        harness_icon(&r.harness),
                        r.name.clone().unwrap_or_else(|| r.harness.clone()),
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
                b.line(
                    if tracked.is_some() {
                        "[enter] focus  [a] answer  [r] reply  [t] task details  [esc] close"
                    } else {
                        "[enter] focus  [a] answer  [r] reply  [t] track this work  [esc] close"
                    },
                    t.dim(),
                );
            }
            Popup::Card { interaction, sel } => {
                let (mi, it) = find_interaction(app, interaction)?;
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
                let who = run
                    .map(|r| r.name.clone().unwrap_or_else(|| r.harness.clone()))
                    .unwrap_or_default();
                let kind = match it.kind {
                    InteractionKind::Approval => "needs approval",
                    InteractionKind::Question => "question",
                    InteractionKind::PlanReview => "plan review",
                    InteractionKind::Notice => "notice",
                };
                let mut b = frame(
                    app,
                    g,
                    80,
                    22,
                    &format!("{who} · {ws} · {kind} · {}", it.handle),
                );
                let mut lines = Vec::new();
                card_lines(app, mi, &it, *sel, &mut lines);
                for (s, st) in lines {
                    b.line(&s, st);
                }
                let keys = if !it.answerable {
                    "this dialog can only be answered in the pane · [o] open pane  [esc] later"
                        .to_string()
                } else {
                    match it.kind {
                        InteractionKind::Approval => "[y] allow once  [s] allow for session  [n] deny  [e] deny with message  [o] open  [esc] later".into(),
                        InteractionKind::PlanReview => "[y] approve  [e] request changes  [n] reject  [o] open  [esc] later".into(),
                        InteractionKind::Question => "[j/k] choose  [enter] answer  [1-9] pick  [o] open  [esc] later".into(),
                        InteractionKind::Notice => "[o] open  [esc] close".into(),
                    }
                };
                b.line("", t.text());
                b.line(&keys, t.dim());
            }
        },
        _ => {}
    }
    None
}

/// The body of an interaction card (shared by the card popup and the inbox detail).
pub fn card_lines(
    app: &App,
    _mi: usize,
    it: &Interaction,
    sel: usize,
    out: &mut Vec<(String, Style)>,
) {
    let t = app.theme;
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
            for l in c.lines().take(4) {
                b.line(&format!("  {l}"), t.text());
            }
        }
        for p in a.paths.iter().take(3) {
            b.line(&format!("  {p}"), t.text());
        }
        if !a.risk_reasons.is_empty() {
            b.line(&format!("reasons: {}", a.risk_reasons.join(", ")), t.dim());
        }
        if let Some(d) = &a.diff {
            for l in d.lines().take(8) {
                let c = if l.starts_with('+') {
                    t.green
                } else if l.starts_with('-') {
                    t.red
                } else {
                    t.muted
                };
                b.line(l, t.s(c));
            }
        }
    } else {
        b.line(&it.title, t.bold(t.fg));
    }
    if let Some(plan) = &it.plan_md {
        for l in plan.lines().take(10) {
            b.line(l, t.text());
        }
    }
    for q in &it.questions {
        b.line(&q.prompt, t.bold(t.fg));
        for (i, o) in q.options.iter().enumerate() {
            let st = if i == sel { t.sel(t.accent) } else { t.text() };
            let d = o
                .description
                .as_ref()
                .map(|d| format!(" — {d}"))
                .unwrap_or_default();
            b.line(&format!("{}. {}{d}", i + 1, o.label), st);
        }
    }
    if it.source == StateSource::Screen {
        b.line(
            &format!("inferred from screen ({:.2})", it.confidence),
            t.dim(),
        );
    }
    if let Some(t) = crate::gateway::interaction_answered_text(it) {
        b.line(&format!("📱 {t}"), app.theme.s(app.theme.accent));
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

struct Lines<'a>(&'a mut Vec<(String, Style)>);

impl Lines<'_> {
    fn line(&mut self, s: &str, st: Style) {
        self.0.push((s.to_string(), st));
    }
}
