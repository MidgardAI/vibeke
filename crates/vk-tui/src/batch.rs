//! Batch approvals view (08 §8, 15 §8.3): answer several equivalent pending approvals at once.
//!
//! Opened with `A` on an interaction card or the palette (`batch_approvals`); off with
//! `ui.interactions.batch = false`. Fingerprints only nominate: approvals are grouped only when
//! they are open, answerable through a **native** channel, of known risk ≤ medium, and agree on
//! machine, harness, tool, the raw command (byte for byte, no normalization), resource targets
//! (paths), execution environment (the pane's isolation level and network profile) and policy
//! scope (the workspace root). The same rule as `vk_review::attention::batchable`, applied to
//! the model the client has. Questions, plan reviews, notices, high/unknown risk,
//! keystroke-only approvals and compound commands (a comment, a line break, `;`, `&&`, `||`, a
//! pipe, backticks or `$(`) are listed as "answer one by one" and never batched.
//!
//! "Allow all N" revalidates every member against the current model right before sending (still
//! open, still answerable, still equivalent) and sends one `interaction.answer` per interaction
//! with its own idempotency key. Each row then shows its own delivery state from the model
//! (`delivering…`, `✓ delivered`, `✗ failed: …`, `? unknown`), so a partial failure is visible.

use crate::app::{App, Mode, Pending, Popup, RpcErr};
use crate::screen::Grid;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use vk_proto::input::{Key, KeyEvent, KeyKind, NamedKey};
use vk_proto::model::*;

pub type Id = (usize, String);
/// Batchable groups (key, members) and the interactions answered one by one (with why).
pub type Groups = (Vec<(String, Vec<Id>)>, Vec<(Id, &'static str)>);

#[derive(Debug, Clone, Default)]
pub struct View {
    pub sel: usize,
    /// Rows the user unticked (default: every member of a group is ticked).
    pub unticked: HashSet<Id>,
    /// Interactions this view answered, with the decision (rows stay until `esc`).
    pub sent: HashMap<Id, String>,
    /// Errors returned by `interaction.answer` per row.
    pub errors: HashMap<Id, String>,
    /// Group order snapshot so answered rows keep their place.
    pub order: Vec<(String, Vec<Id>)>,
    pub notice: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Reply {
    Answered(Id),
}

/// Whether a command may share a batch at all (08 §8): one plain command. Anything with a
/// comment (`#`), a line break, `;`, `&&`, `||`, a pipe, backticks or `$(` is answered one by
/// one, because what a reviewer sees of the first command says nothing reliable about the
/// rest of another.
pub fn batch_safe_command(cmd: &str) -> bool {
    const UNSAFE: &[&str] = &["#", "\n", "\r", ";", "&&", "||", "|", "`", "$("];
    !UNSAFE.iter().any(|t| cmd.contains(t))
}

/// Why an interaction can't be batched, or its equivalence key.
pub fn equivalence(app: &App, mi: usize, it: &Interaction) -> Result<String, &'static str> {
    if !app.machines[mi].capabilities().approve {
        return Err("this shared session is view only");
    }
    if it.kind != InteractionKind::Approval {
        return Err("questions and plan reviews are answered one by one");
    }
    if it.status != InteractionStatus::Open || !it.answerable {
        return Err("can only be answered in the pane");
    }
    if it.answer_channel != AnswerChannel::Native {
        return Err("keystroke delivery: answer one by one");
    }
    let Some(a) = &it.action else {
        return Err("unknown action");
    };
    match a.risk {
        Risk::Low | Risk::Medium => {}
        Risk::High => return Err("high risk"),
        Risk::Unknown => return Err("unknown risk"),
    }
    let m = &app.machines[mi];
    let harness = m
        .model
        .runs
        .iter()
        .find(|r| r.id == it.run)
        .map(|r| r.harness.clone())
        .unwrap_or_default();
    let pane = m.model.panes.iter().find(|p| p.id == it.pane);
    let env = pane
        .map(|p| format!("{:?}/{}", p.isolation.level, p.isolation.network))
        .unwrap_or_default();
    let scope = pane
        .and_then(|p| m.model.workspaces.iter().find(|w| w.id == p.workspace))
        .map(|w| w.root_path.clone())
        .unwrap_or_default();
    // The raw command, byte for byte: no whitespace normalization (`echo a # rm x` and
    // `echo a #\nrm x` are different programs), and anything that can chain, comment out or
    // substitute is never batched at all.
    let cmd = a.command.as_deref().unwrap_or(&a.summary);
    if !batch_safe_command(cmd) {
        return Err(
            "compound command (comment, newline, ;, &&, ||, |, `…` or $(…)): answer one by one",
        );
    }
    let mut paths = a.paths.clone();
    paths.sort();
    paths.dedup();
    Ok(format!(
        "{}\u{1f}{harness}\u{1f}{}\u{1f}{cmd}\u{1f}{}\u{1f}{env}\u{1f}{scope}",
        m.label,
        a.tool,
        paths.join("\u{1e}")
    ))
}

fn find(app: &App, id: &Id) -> Option<Interaction> {
    app.machines
        .get(id.0)?
        .model
        .interactions
        .iter()
        .find(|i| i.id == id.1)
        .cloned()
}

/// Groups of ≥ 2 equivalent approvals (key, members oldest first), then the open interactions
/// that can't be batched with their reason.
pub fn groups(app: &App) -> Groups {
    let mut by: BTreeMap<String, Vec<(i64, Id)>> = BTreeMap::new();
    let mut alone = Vec::new();
    for (mi, it) in crate::popups::inbox(app) {
        match equivalence(app, mi, &it) {
            Ok(k) => by
                .entry(k)
                .or_default()
                .push((it.opened_at_ms, (mi, it.id.clone()))),
            Err(why) => alone.push(((mi, it.id.clone()), why)),
        }
    }
    let mut out = Vec::new();
    for (k, mut v) in by {
        if v.len() < 2 {
            for (_, id) in v {
                alone.push((id, "nothing equivalent is waiting"));
            }
            continue;
        }
        v.sort_by_key(|x| x.0);
        out.push((k, v.into_iter().map(|x| x.1).collect::<Vec<Id>>()));
    }
    out.sort_by_key(|(_, v)| v.first().map(|id| find(app, id).map(|i| i.opened_at_ms)));
    (out, alone)
}

pub fn open(app: &mut App, seed: Option<Id>) {
    if !app.config.ui.interactions.batch {
        app.toast("the batch view is off (ui.interactions.batch = false)");
        return;
    }
    let (order, _) = groups(app);
    let mut v = View {
        order,
        ..Default::default()
    };
    if let Some(seed) = seed {
        // Start on the seed's group.
        let rows = rows(app, &v);
        v.sel = rows
            .iter()
            .position(|r| matches!(r, Row::Member(_, id) if *id == seed))
            .unwrap_or(0);
    }
    if v.order.is_empty() {
        v.notice = Some("no equivalent approvals are waiting — answer them one by one".into());
    }
    app.ux.batch = Some(v);
    app.mode = Mode::Popup(Popup::Batch);
}

pub fn action(app: &mut App, action: &str) -> bool {
    if action == "batch_approvals" {
        open(app, None);
        return true;
    }
    false
}

/// Display rows: group headers and members.
#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    Header(usize),
    Member(usize, Id),
}

fn rows(_app: &App, v: &View) -> Vec<Row> {
    let mut out = Vec::new();
    for (gi, (_, ids)) in v.order.iter().enumerate() {
        out.push(Row::Header(gi));
        for id in ids {
            out.push(Row::Member(gi, id.clone()));
        }
    }
    out
}

fn group_of(rows: &[Row], sel: usize) -> Option<usize> {
    match rows.get(sel)? {
        Row::Header(g) | Row::Member(g, _) => Some(*g),
    }
}

/// Answer the ticked members of group `gi` with `decision`, each revalidated first.
pub fn answer_group(app: &mut App, v: &mut View, gi: usize, decision: &str) {
    let Some((key, ids)) = v.order.get(gi).cloned() else {
        return;
    };
    let (mut sent, mut skipped) = (0, 0);
    for id in ids {
        if v.unticked.contains(&id) || v.sent.contains_key(&id) {
            continue;
        }
        // Live revalidation: still open, answerable and equivalent to the group.
        let ok =
            find(app, &id).is_some_and(|it| equivalence(app, id.0, &it).as_deref() == Ok(&key));
        if !ok {
            skipped += 1;
            v.errors
                .insert(id.clone(), "no longer pending or changed — skipped".into());
            continue;
        }
        let p = app.answer_params(id.0, &id.1, json!({"decision": decision}), "batch-");
        app.command_on(
            id.0,
            "interaction.answer",
            p,
            Pending::Ux(crate::ux::Reply::Batch(Reply::Answered(id.clone()))),
        );
        v.sent.insert(id, decision.to_string());
        sent += 1;
    }
    v.notice = Some(match (sent, skipped) {
        (n, 0) => format!("{decision}: sent {n} answer(s) — each delivery shows on its row"),
        (n, s) => format!("{decision}: sent {n}, skipped {s} that changed"),
    });
}

pub fn on_reply(app: &mut App, _mi: usize, r: Reply, res: Result<Value, RpcErr>) {
    let Reply::Answered(id) = r;
    if let (Err(e), Some(v)) = (res, app.ux.batch.as_mut()) {
        v.errors.insert(id, e.message);
    }
    app.dirty = true;
}

pub fn key(app: &mut App, ev: KeyEvent) {
    let keep = |app: &mut App| app.mode = Mode::Popup(Popup::Batch);
    if ev.kind == KeyKind::Release {
        return keep(app);
    }
    let Some(mut v) = app.ux.batch.take() else {
        return;
    };
    let rs = rows(app, &v);
    let n = rs.len();
    match ev.key {
        Key::Named(NamedKey::Escape) | Key::Char('q') => return,
        Key::Char('j') | Key::Named(NamedKey::Down) => v.sel = (v.sel + 1).min(n.saturating_sub(1)),
        Key::Char('k') | Key::Named(NamedKey::Up) => v.sel = v.sel.saturating_sub(1),
        Key::Char(' ') => {
            if let Some(Row::Member(_, id)) = rs.get(v.sel)
                && !v.unticked.remove(id)
            {
                v.unticked.insert(id.clone());
            }
        }
        Key::Char('a' | 'y') => {
            if let Some(g) = group_of(&rs, v.sel) {
                answer_group(app, &mut v, g, "allow");
            }
        }
        Key::Char('n') => {
            if let Some(g) = group_of(&rs, v.sel) {
                answer_group(app, &mut v, g, "deny");
            }
        }
        Key::Char('o') | Key::Named(NamedKey::Enter) => {
            if let Some(Row::Member(_, id)) = rs.get(v.sel) {
                let pane = find(app, id).map(|i| i.pane);
                if let Some(p) = pane {
                    app.focus_pane(id.0, &p);
                    return;
                }
            }
        }
        _ => {}
    }
    app.ux.batch = Some(v);
    keep(app);
}

fn delivery_text(it: &Interaction) -> (String, char) {
    match it.delivery {
        DeliveryState::DecisionRecorded | DeliveryState::Delivering => ("delivering…".into(), 'y'),
        DeliveryState::Delivered => ("✓ delivered".into(), 'g'),
        DeliveryState::DeliveryUnknown => ("? unknown — check the pane".into(), 'r'),
        DeliveryState::Failed => (
            format!(
                "✗ failed: {}",
                it.delivery_error.clone().unwrap_or_default()
            ),
            'r',
        ),
        _ => match it.status {
            InteractionStatus::Open => ("waiting".into(), 'm'),
            _ => (
                format!(
                    "answered{}",
                    it.answered_by
                        .as_ref()
                        .map(|b| format!(" by {b}"))
                        .unwrap_or_default()
                ),
                'm',
            ),
        },
    }
}

pub fn draw(app: &App, g: &mut Grid) {
    let Some(v) = &app.ux.batch else {
        return;
    };
    let t = app.theme;
    let (_, alone) = groups(app);
    let total: usize = v.order.iter().map(|(_, ids)| ids.len()).sum();
    let mut a = crate::drafts::Area::open(
        app,
        g,
        &format!(
            "batch approvals · {total} equivalent in {} group(s)",
            v.order.len()
        ),
    );
    if let Some(n) = &v.notice {
        a.line(n, t.bold(t.yellow));
    }
    for (i, row) in rows(app, v).iter().enumerate() {
        let selected = i == v.sel;
        match row {
            Row::Header(gi) => {
                let ids = &v.order[*gi].1;
                let it = ids.first().and_then(|id| find(app, id));
                let ticked = ids
                    .iter()
                    .filter(|id| !v.unticked.contains(*id) && !v.sent.contains_key(*id))
                    .count();
                let what = it
                    .as_ref()
                    .and_then(|i| i.action.as_ref())
                    .map(|a| {
                        let risk = match a.risk {
                            Risk::Low => "low",
                            Risk::Medium => "medium",
                            _ => "?",
                        };
                        format!(
                            "{} `{}` · risk {risk}",
                            a.tool,
                            a.command.as_deref().unwrap_or(&a.summary)
                        )
                    })
                    .unwrap_or_default();
                let st = if selected {
                    t.sel(t.accent)
                } else {
                    t.bold(t.fg)
                };
                a.line(
                    &format!("── {what} · [a] allow all {ticked} · [n] deny all ──"),
                    st,
                );
            }
            Row::Member(_, id) => {
                let Some(it) = find(app, id) else {
                    a.line("   (gone)", t.dim());
                    continue;
                };
                let m = &app.machines[id.0];
                let who = m
                    .model
                    .runs
                    .iter()
                    .find(|r| r.id == it.run)
                    .map(|r| r.label().to_string())
                    .unwrap_or_default();
                let ws = m
                    .model
                    .panes
                    .iter()
                    .find(|p| p.id == it.pane)
                    .and_then(|p| m.model.workspaces.iter().find(|w| w.id == p.workspace))
                    .map(|w| w.display_name().to_string())
                    .unwrap_or_default();
                let mark = if v.sent.contains_key(id) {
                    "[·]"
                } else if v.unticked.contains(id) {
                    "[ ]"
                } else {
                    "[x]"
                };
                let (state, c) = match v.errors.get(id) {
                    Some(e) => (format!("✗ {e}"), 'r'),
                    None => delivery_text(&it),
                };
                let color = match c {
                    'g' => t.green,
                    'r' => t.red,
                    'y' => t.yellow,
                    _ => t.muted,
                };
                let multi = if app.machines.len() > 1 {
                    format!("{} ", m.label)
                } else {
                    String::new()
                };
                let text = format!(" {mark} {} {multi}{who} · {ws}   {state}", it.handle);
                a.line(&text, if selected { t.sel(color) } else { t.s(color) });
            }
        }
    }
    if !alone.is_empty() {
        a.line("", t.text());
        a.line("── answer one by one ──", t.dim());
        for (id, why) in alone {
            if let Some(it) = find(app, &id) {
                a.line(&format!("   {} {} — {why}", it.handle, it.title), t.dim());
            }
        }
    }
    a.footer(
        "j/k move · space tick/untick · a allow all ticked · n deny all ticked · o open pane · esc back",
        t.dim(),
    );
}

#[cfg(test)]
#[path = "batch_tests.rs"]
mod tests;
