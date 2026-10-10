//! The prefix menu (08 §10.4): after the prefix, every prefix key in groups, magit-style.
//! Key sequences (`prefix+m w`) draw as submenus (`▸`) and resize mode draws as a sticky
//! level. Items come from the live keymap, so rebinds, unbinds, ranges, `[[keys.command]]`
//! entries and plugin bindings all show without any extra registration.

use crate::app::{App, Mode};
use crate::keymap::{LevelEntry, key_matches};
use crate::screen::{Grid, Rect as SRect};
use unicode_width::UnicodeWidthStr;
use vk_proto::input::KeyEvent;
use vk_term::keygrammar::{display_key, parse_binding};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub title: String,
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// Display form of the chord (`N`, `alt+d`, `1‥9`).
    pub key: String,
    pub label: String,
    /// Opens a submenu or a sticky level (drawn with `▸`).
    pub submenu: bool,
    /// Position in `GROUPS` (bindings arrive sorted by action name).
    order: usize,
}

/// Built-in actions by group, in display order. Anything else bound under the prefix lands in
/// `commands` (`[[keys.command]]`, repo commands), `plugins` or `other`.
const GROUPS: &[(&str, &[&str])] = &[
    (
        "pane",
        &[
            "split_vertical",
            "split_horizontal",
            "close_pane",
            "zoom",
            "resize_mode",
            "float_new",
            "toggle_floats",
            "pin_pane",
            "rename_pane",
            "remote_image_paste",
        ],
    ),
    (
        "focus",
        &[
            "focus_pane_left",
            "focus_pane_down",
            "focus_pane_up",
            "focus_pane_right",
            "cycle_pane_next",
            "cycle_pane_previous",
            "last_pane",
        ],
    ),
    (
        "text",
        &[
            "enter_copy_mode",
            "paste_buffer",
            "search_scrollback",
            "edit_scrollback",
            "url_hints",
            "review_clipboard",
        ],
    ),
    (
        "tab",
        &[
            "new_tab",
            "next_tab",
            "previous_tab",
            "switch_tab",
            "rename_tab",
            "close_tab",
            "tab_renumber",
            "sync_input",
            "sync_input_pane",
            "sync_input_off",
        ],
    ),
    (
        "workspace",
        &[
            "workspace_picker",
            "goto",
            "toggle_sidebar",
            "new_workspace",
            "new_worktree",
            "new_task",
            "open_worktree",
            "remove_worktree",
            "last_workspace",
            "previous_workspace",
            "next_workspace",
            "switch_workspace",
            "rename_workspace",
            "close_workspace",
            "preview_list",
        ],
    ),
    (
        "agents",
        &[
            "next_attention",
            "next_attention_focus",
            "inbox",
            "agent_list",
            "fleet",
            "mark_unread",
            "open_notification_target",
            "previous_agent",
            "next_agent",
            "focus_agent",
            "batch_approvals",
            "collisions",
        ],
    ),
    (
        "connect",
        &[
            "connections",
            "share_pane",
            "handoff_send",
            "cloud_send",
            "cloud_bring_back",
            "sandboxes",
            "handoffs",
            "devices",
            "people",
            "hosts",
            "elevation_requests",
            "cancel_transfer",
        ],
    ),
    (
        "session",
        &[
            "command_palette",
            "search_global",
            "help",
            "settings",
            "setup",
            "trust_repo",
            "reload_config",
            "detach",
        ],
    ),
];

/// Short labels (the palette's descriptions are too long for a column).
const LABELS: &[(&str, &str)] = &[
    ("split_vertical", "split side by side"),
    ("split_horizontal", "split stacked"),
    ("close_pane", "close"),
    ("zoom", "zoom"),
    ("focus_pane_left", "focus left"),
    ("focus_pane_down", "focus down"),
    ("focus_pane_up", "focus up"),
    ("focus_pane_right", "focus right"),
    ("cycle_pane_next", "next pane"),
    ("cycle_pane_previous", "previous pane"),
    ("last_pane", "last pane"),
    ("resize_mode", "resize"),
    ("float_new", "float"),
    ("toggle_floats", "show/hide floats"),
    ("rename_pane", "rename pane"),
    ("pin_pane", "pin"),
    ("edit_scrollback", "edit scrollback"),
    ("enter_copy_mode", "copy mode"),
    ("paste_buffer", "paste"),
    ("search_scrollback", "search scrollback"),
    ("url_hints", "url hints"),
    ("remote_image_paste", "paste image"),
    ("new_tab", "new tab"),
    ("next_tab", "next tab"),
    ("previous_tab", "previous tab"),
    ("switch_tab", "jump to tab"),
    ("close_tab", "close tab"),
    ("rename_tab", "rename tab"),
    ("tab_renumber", "renumber tabs"),
    ("sync_input", "sync input"),
    ("sync_input_pane", "sync this pane"),
    ("sync_input_off", "stop sync input"),
    ("workspace_picker", "sidebar"),
    ("goto", "goto anything"),
    ("toggle_sidebar", "show/hide sidebar"),
    ("new_workspace", "new workspace"),
    ("new_worktree", "new worktree"),
    ("open_worktree", "open worktree"),
    ("remove_worktree", "remove worktree"),
    ("new_task", "new task"),
    ("last_workspace", "last workspace"),
    ("previous_workspace", "previous workspace"),
    ("next_workspace", "next workspace"),
    ("switch_workspace", "jump to workspace"),
    ("rename_workspace", "rename workspace"),
    ("close_workspace", "close workspace"),
    ("preview_list", "previews"),
    ("next_attention", "next needing you"),
    ("next_attention_focus", "focus next needing you"),
    ("inbox", "inbox"),
    ("agent_list", "agent list"),
    ("previous_agent", "previous agent"),
    ("next_agent", "next agent"),
    ("focus_agent", "jump to agent"),
    ("fleet", "fleet"),
    ("mark_unread", "mark unread"),
    ("open_notification_target", "open notification"),
    ("batch_approvals", "batch approvals"),
    ("elevation_requests", "elevation requests"),
    ("collisions", "collisions"),
    ("connections", "connections"),
    ("share_pane", "share pane"),
    ("handoffs", "handoffs"),
    ("handoff_send", "hand off pane"),
    ("cloud_send", "send to cloud"),
    ("cloud_bring_back", "bring back from cloud"),
    ("sandboxes", "sandboxes"),
    ("devices", "devices"),
    ("people", "people"),
    ("hosts", "hosts"),
    ("review_clipboard", "review clipboard"),
    ("cancel_transfer", "cancel transfer"),
    ("command_palette", "palette"),
    ("search_global", "search everywhere"),
    ("settings", "settings"),
    ("setup", "setup"),
    ("trust_repo", "trust repo"),
    ("reload_config", "reload config"),
    ("detach", "detach"),
    ("help", "this menu"),
    ("browser_address", "address bar"),
    ("browser_back", "back"),
    ("browser_forward", "forward"),
    ("browser_reload", "reload"),
    ("browser_hard_reload", "hard reload"),
    ("browser_screenshot", "screenshot"),
    ("browser_window", "open in your browser"),
    ("browser_console", "console"),
    ("browser_take_over", "take control"),
    ("browser_paste_image", "paste image"),
];

/// Actions that open a sticky level of their own.
const STICKY: &[&str] = &["resize_mode"];

const ORDER: &[&str] = &[
    "browser",
    "pane",
    "focus",
    "text",
    "tab",
    "workspace",
    "agents",
    "connect",
    "session",
    "commands",
    "plugins",
    "other",
];

/// Column gap between groups.
const GAP: usize = 3;

/// The groups of the level under `seq` (empty: the top level) for the current keymap and
/// focus. Empty groups are left out.
pub fn build(app: &App, seq: &[KeyEvent]) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    // A browser pane's own table wins over global keys on the same chords.
    let browser: Vec<(KeyEvent, &str)> = if seq.is_empty() {
        browser_keys(app)
    } else {
        Vec::new()
    };
    for (k, action) in &browser {
        push(
            &mut groups,
            "browser",
            item(&display_key(k), &label(app, action), false),
        );
    }
    // Ranged bindings (`switch_tab = prefix+1..9`) collapse into one `1‥9` item.
    let mut ranged: Vec<(String, (usize, usize))> = Vec::new();
    for (k, entry) in app.keymap.level(seq) {
        if browser.iter().any(|(bk, _)| key_matches(bk, &k)) {
            continue;
        }
        match entry {
            LevelEntry::Action(b) => {
                if b.index.is_some()
                    && let Some((_, (gi, ii))) = ranged.iter().find(|(a, _)| *a == b.action)
                {
                    let it = &mut groups[*gi].items[*ii];
                    let first = it.key.split('‥').next().unwrap_or_default().to_string();
                    it.key = format!("{first}‥{}", display_key(&k));
                    continue;
                }
                let mut it = item(
                    &display_key(&k),
                    &label(app, &b.action),
                    STICKY.contains(&b.action.as_str()),
                );
                it.order = order_of(&b.action);
                let at = push(&mut groups, group_of(&b.action), it);
                if b.index.is_some() {
                    ranged.push((b.action.clone(), at));
                }
            }
            LevelEntry::Submenu(n) => {
                let mut deeper = seq.to_vec();
                deeper.push(k.clone());
                let first = app
                    .keymap
                    .level(&deeper)
                    .into_iter()
                    .find_map(|(_, e)| match e {
                        LevelEntry::Action(b) => Some(b.action.clone()),
                        LevelEntry::Submenu(_) => None,
                    });
                let title = first.as_deref().map(group_of).unwrap_or("other");
                let label = if n == 1 {
                    "1 key".to_string()
                } else {
                    format!("{n} keys")
                };
                let mut it = item(&display_key(&k), &label, true);
                it.order = first.as_deref().map(order_of).unwrap_or(usize::MAX);
                push(&mut groups, title, it);
            }
        }
    }
    for g in &mut groups {
        g.items.sort_by_key(|i| i.order);
    }
    groups.sort_by_key(|g| {
        ORDER
            .iter()
            .position(|o| *o == g.title)
            .unwrap_or(ORDER.len())
    });
    groups
}

fn item(key: &str, label: &str, submenu: bool) -> Item {
    Item {
        key: key.to_string(),
        label: label.to_string(),
        submenu,
        order: usize::MAX,
    }
}

fn order_of(action: &str) -> usize {
    GROUPS
        .iter()
        .flat_map(|(_, actions)| actions.iter())
        .position(|a| *a == action)
        .unwrap_or(usize::MAX)
}

fn push(groups: &mut Vec<Group>, title: &str, it: Item) -> (usize, usize) {
    let gi = match groups.iter().position(|g| g.title == title) {
        Some(gi) => gi,
        None => {
            groups.push(Group {
                title: title.to_string(),
                items: Vec::new(),
            });
            groups.len() - 1
        }
    };
    groups[gi].items.push(it);
    (gi, groups[gi].items.len() - 1)
}

fn group_of(action: &str) -> &'static str {
    if action.starts_with("command:") || action.starts_with("repo:") {
        return "commands";
    }
    if action.starts_with("plugin:") {
        return "plugins";
    }
    if action.starts_with("browser_") {
        return "browser";
    }
    GROUPS
        .iter()
        .find(|(_, actions)| actions.contains(&action))
        .map(|(title, _)| *title)
        .unwrap_or("other")
}

/// The browser prefix table while a browser pane is focused (config overrides applied).
fn browser_keys(app: &App) -> Vec<(KeyEvent, &'static str)> {
    if crate::browser::focused_browser(app).is_none() {
        return Vec::new();
    }
    crate::browser::DEFAULT_BROWSER_KEYS
        .iter()
        .filter_map(|(action, default)| {
            let spec = app
                .config
                .keys
                .bindings
                .get(*action)
                .map(String::as_str)
                .unwrap_or(default);
            let b = parse_binding(spec).ok()?;
            (b.prefix && b.chords.len() == 1).then(|| (b.chords[0].clone(), *action))
        })
        .collect()
}

/// A short label for an action id.
pub fn label(app: &App, action: &str) -> String {
    if let Some((_, l)) = LABELS.iter().find(|(a, _)| *a == action) {
        return (*l).to_string();
    }
    if let Some(i) = action.strip_prefix("command:")
        && let Some(c) = i
            .parse::<usize>()
            .ok()
            .and_then(|i| app.config.keys.command.get(i))
    {
        return c
            .title
            .clone()
            .or_else(|| c.description.clone())
            .unwrap_or_else(|| c.command.clone());
    }
    if let Some(rest) = action.strip_prefix("plugin:") {
        // `plugin:<machine>:<plugin>.<action>`
        let (mi, qualified) = rest.split_once(':').unwrap_or(("", rest));
        if let Ok(mi) = mi.parse::<usize>()
            && let Some(per) = app.plugins.per.get(&mi)
            && let Some(a) = per.actions.iter().find(|a| a.qualified == qualified)
        {
            return a.title.clone();
        }
        return qualified.to_string();
    }
    if action.starts_with("repo:")
        && let Some((_, d, _)) = crate::trust::palette_entries(app)
            .into_iter()
            .find(|(id, _, _)| id == action)
    {
        return d
            .strip_prefix("Repo command: ")
            .map(str::to_string)
            .unwrap_or(d);
    }
    let mut d = crate::nav::describe(action);
    if let Some(i) = d.find(" (") {
        d.truncate(i);
    }
    let mut c = d.chars();
    match c.next() {
        Some(f) => f.to_lowercase().collect::<String>() + c.as_str(),
        None => d,
    }
}

/// The prefix menu for the current level.
pub fn draw(app: &App, g: &mut Grid) {
    let Mode::Prefix(p) = &app.mode else {
        return;
    };
    let groups = build(app, &p.seq);
    let mut title = display_key(&app.keymap.prefix);
    if !p.seq.is_empty() {
        title.push(' ');
        title.push_str(&p.seq_text());
    }
    let footer = if p.seq.is_empty() {
        "esc close · : palette"
    } else {
        "esc back"
    };
    draw_groups(app, g, &title, &groups, footer);
}

/// Resize mode as a sticky level: the keys repeat until Esc.
pub fn draw_resize(app: &App, g: &mut Grid) {
    let floating = crate::floats::focused_float(app).is_some();
    let items: Vec<(&str, &str)> = if floating {
        vec![
            ("h j k l", "nudge 2 (arrows too)"),
            ("H J K L", "nudge 10"),
            (
                "m",
                if app.parity.floats.move_mode {
                    "resize instead of move"
                } else {
                    "move instead of resize"
                },
            ),
            ("=", "centre"),
        ]
    } else {
        vec![
            ("h j k l", "resize 5% (arrows too)"),
            ("H J K L", "resize 25%"),
            ("=", "equalize"),
        ]
    };
    let groups = vec![Group {
        title: if floating {
            "float".into()
        } else {
            "resize".into()
        },
        items: items.iter().map(|(k, l)| item(k, l, false)).collect(),
    }];
    let key = app
        .keymap
        .bindings
        .iter()
        .find(|b| b.prefix && b.action == "resize_mode" && b.chords.len() == 1)
        .map(|b| display_key(&b.chords[0]));
    let title = match key {
        Some(k) => format!("{} {k} · resize", display_key(&app.keymap.prefix)),
        None => "resize".to_string(),
    };
    draw_groups(app, g, &title, &groups, "keys repeat · esc done");
}

/// Groups stacked into columns (each group goes to the shortest column so far), anchored above
/// the status bar and centred. A screen too short for everything drops the footer first and then
/// the bottom of the tallest column.
fn draw_groups(app: &App, g: &mut Grid, title: &str, groups: &[Group], footer: &str) {
    let area = app.pane_area();
    if area.w < 12 || area.h < 4 || groups.is_empty() {
        return;
    }
    let t = app.theme;
    let inner = area.w.saturating_sub(4) as usize;
    // Per group: key column width, total width.
    let key_w: Vec<usize> = groups
        .iter()
        .map(|gr| gr.items.iter().map(|i| i.key.width()).max().unwrap_or(0))
        .collect();
    let widths: Vec<usize> = groups
        .iter()
        .zip(&key_w)
        .map(|(gr, kw)| {
            let items = gr
                .items
                .iter()
                .map(|i| kw + 1 + i.label.width() + if i.submenu { 2 } else { 0 })
                .max()
                .unwrap_or(0);
            items.max(gr.title.width()).min(inner.max(1))
        })
        .collect();
    // As many columns as fit: each group goes to the shortest column so far (ties go left),
    // and groups keep their order within a column.
    let pack = |ncols: usize| {
        let mut cols: Vec<Vec<usize>> = vec![Vec::new(); ncols];
        let mut heights = vec![0usize; ncols];
        for (gi, gr) in groups.iter().enumerate() {
            let c = (0..ncols).min_by_key(|c| (heights[*c], *c)).unwrap_or(0);
            if !cols[c].is_empty() {
                heights[c] += 1;
            }
            heights[c] += 1 + gr.items.len();
            cols[c].push(gi);
        }
        let col_w: Vec<usize> = cols
            .iter()
            .map(|col| col.iter().map(|gi| widths[*gi]).max().unwrap_or(0))
            .collect();
        (cols, heights, col_w)
    };
    let layout = |inner: usize| {
        (1..=groups.len())
            .rev()
            .map(pack)
            .find(|(_, _, col_w)| {
                col_w.iter().sum::<usize>() + GAP * col_w.len().saturating_sub(1) <= inner
            })
            .unwrap_or_else(|| pack(1))
    };
    let max_h = area.h.saturating_sub(1).max(4);
    let visible = (max_h as usize).saturating_sub(3); // borders + footer
    // Rows past the bottom are not drawn; count the keys that hides.
    let hidden_in = |cols: &[Vec<usize>]| -> usize {
        cols.iter()
            .map(|col| {
                let mut at = 0;
                let mut n = 0;
                for gi in col {
                    let gr = &groups[*gi];
                    n += gr
                        .items
                        .len()
                        .saturating_sub(visible.saturating_sub(at + 1));
                    at += gr.items.len() + 2;
                }
                n
            })
            .sum()
    };
    // The menu is modal: when it does not fit the pane area it may cover the sidebar too.
    let (mut ax, mut aw) = (area.x, area.w);
    let mut packed = layout(inner);
    if hidden_in(&packed.0) > 0 && app.size.0 > area.w {
        (ax, aw) = (0, app.size.0);
        packed = layout(aw.saturating_sub(4) as usize);
    }
    let (cols, heights, col_w) = packed;
    let ncols = cols.len();
    let hidden = hidden_in(&cols);
    let footer = if hidden > 0 {
        let esc = footer
            .split(" · ")
            .find(|s| s.starts_with("esc"))
            .unwrap_or(footer);
        format!("{hidden} more · : palette has everything · {esc}")
    } else {
        footer.to_string()
    };
    let body = heights.iter().copied().max().unwrap_or(0);
    let content_w = (col_w.iter().sum::<usize>() + GAP * ncols.saturating_sub(1))
        .max(title.width() + 2)
        .max(footer.width());
    let h = ((body + 3) as u16).min(max_h);
    let w = ((content_w + 4) as u16).min(aw).max(20.min(aw));
    let x = ax + (aw - w) / 2;
    let y = area.y + area.h - h;
    let mut b = crate::popups::frame_at(app, g, SRect { x, y, w, h }, title);
    for row in 0..visible.min(body) {
        let mut dx = 0;
        for (c, col) in cols.iter().enumerate() {
            // Which group, and which line of it, sits on this row of the column?
            let mut at = 0;
            for gi in col {
                let gr = &groups[*gi];
                let lines = 1 + gr.items.len();
                if row >= at && row < at + lines {
                    let line = row - at;
                    if line == 0 {
                        b.put(dx as u16, &gr.title.to_uppercase(), t.dim());
                    } else if let Some(it) = gr.items.get(line - 1) {
                        b.put(dx as u16, &it.key, t.bold(t.accent));
                        let lx = dx + key_w[*gi] + 1;
                        b.put(lx as u16, &it.label, t.text());
                        if it.submenu {
                            b.put((lx + it.label.width() + 1) as u16, "▸", t.dim());
                        }
                    }
                    break;
                }
                at += lines + 1;
            }
            dx += col_w[c] + GAP;
        }
        b.next();
    }
    b.line(&footer, t.dim());
}
