//! Native plugin UI contributions in the TUI (07 §7.4; lane 3B). The server validates and
//! sanitizes every contribution and lists them in `compat.ui.state.contributions` (re-read on
//! `ui.contributions_changed`); the TUI only draws data, sanitizing again:
//!
//! - **Status segments** are appended to the status bar's left or right list by `priority`
//!   (highest first); a click runs the segment's `on_click` action.
//! - **Sidebar sections** are drawn above the pinned section as `─ title ─` with one row per
//!   item (`label`, `badge`, `state_color`); enter/click on an item runs `on_select`, on the
//!   header collapses or expands the section (kept per client session).
//! - **Pane decorations** add a badge and title suffix to the pane's sidebar rows and color the
//!   pane's border cells (`border_color`).
//! - Palette commands, contributed panes and key bindings arrive through `plugin.action.list`
//!   like manifest actions, so the palette and keymap code needs nothing extra.

use crate::app::App;
use crate::draw::SideRow;
use serde_json::Value;
use vk_proto::layout::Rect;
use vk_proto::render::Style;

/// Group-id prefixes of plugin sidebar rows (navigate mode treats them like group rows).
const SECTION: &str = "plugin-section\u{1f}";
const ITEM: &str = "plugin-item\u{1f}";

/// Contributions from a `compat.ui.state` reply.
pub fn parse(v: &Value) -> Vec<Value> {
    v["contributions"].as_array().cloned().unwrap_or_default()
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or("")
}

fn clean(t: &str, max: usize) -> String {
    crate::plugins::sanitize(t, max)
}

fn of_kind<'a>(app: &'a App, mi: usize, kind: &'a str) -> impl Iterator<Item = &'a Value> + 'a {
    app.plugins
        .per(mi)
        .map(|p| p.contrib.as_slice())
        .unwrap_or_default()
        .iter()
        .filter(move |c| c["kind"] == kind)
}

fn style_for(app: &App, color: &str) -> Style {
    match crate::sidebar::color(app, color) {
        Some(c) => app.theme.s(c),
        None => app.theme.text(),
    }
}

/// Plugin status segments of the focused machine for one side: (id, text, style), highest
/// priority first. The id is `plugin:<plugin>:<segment>`.
pub fn segments(app: &App, side: &str) -> Vec<(String, String, Style)> {
    let mut v: Vec<(i64, String, String, Style)> = of_kind(app, app.cur, "status_segment")
        .filter(|c| s(c, "side") == side || (side == "right" && s(c, "side").is_empty()))
        .map(|c| {
            (
                c["priority"].as_i64().unwrap_or(0),
                format!("plugin:{}:{}", s(c, "plugin_id"), s(c, "id")),
                clean(s(c, "text"), 40),
                style_for(app, s(c, "color")),
            )
        })
        .filter(|x| !x.2.is_empty())
        .collect();
    v.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    v.into_iter().map(|(_, id, t, st)| (id, t, st)).collect()
}

/// A click on a plugin status segment: run its `on_click` action.
pub fn click_segment(app: &mut App, id: &str) -> bool {
    let Some(rest) = id.strip_prefix("plugin:") else {
        return false;
    };
    let Some((plugin, seg)) = rest.split_once(':') else {
        return false;
    };
    let mi = app.cur;
    let hit = of_kind(app, mi, "status_segment")
        .find(|c| s(c, "plugin_id") == plugin && s(c, "id") == seg)
        .map(|c| (s(c, "on_click").to_string(), clean(s(c, "text"), 40)));
    if let Some((action, text)) = hit.filter(|(a, _)| !a.is_empty()) {
        crate::plugins::run_action(app, mi, plugin, Some(&action), &text, "status");
    }
    true
}

/// Sidebar rows of every machine's plugin sections (before the pinned section).
pub fn push_sidebar(app: &App, rows: &mut Vec<SideRow>) {
    let t = &app.theme;
    for mi in 0..app.machines.len() {
        let mut sections: Vec<&Value> = of_kind(app, mi, "sidebar_section").collect();
        sections.sort_by_key(|c| (c["order"].as_i64().unwrap_or(0), s(c, "id").to_string()));
        for sec in sections {
            let key = format!("{}\u{1f}{}", s(sec, "plugin_id"), s(sec, "id"));
            let collapsed = app.plugins.collapsed.contains(&key);
            rows.push(SideRow {
                segs: vec![(
                    format!(
                        "{} {} ─",
                        if collapsed { "▸" } else { "▾" },
                        clean(s(sec, "title"), 40)
                    ),
                    t.dim(),
                )],
                group: Some((mi, format!("{SECTION}{key}"))),
                ..Default::default()
            });
            if collapsed {
                continue;
            }
            for it in sec["items"].as_array().cloned().unwrap_or_default() {
                let mut segs = vec![(
                    format!("  {}", clean(s(&it, "label"), 60)),
                    style_for(app, s(&it, "state_color")),
                )];
                let badge = clean(s(&it, "badge"), 12);
                if !badge.is_empty() {
                    segs.push((format!(" [{badge}]"), t.s(t.accent)));
                }
                rows.push(SideRow {
                    segs,
                    group: Some((mi, format!("{ITEM}{key}\u{1f}{}", s(&it, "id")))),
                    ..Default::default()
                });
            }
        }
    }
}

/// Enter/click on a plugin sidebar row (a "group" id from [`push_sidebar`]). True when handled.
pub fn activate(app: &mut App, mi: usize, gid: &str) -> bool {
    if let Some(key) = gid.strip_prefix(SECTION) {
        let key = key.to_string();
        if !app.plugins.collapsed.remove(&key) {
            app.plugins.collapsed.insert(key);
        }
        app.dirty = true;
        return true;
    }
    let Some(rest) = gid.strip_prefix(ITEM) else {
        return false;
    };
    let mut parts = rest.split('\u{1f}');
    let (Some(plugin), Some(section), Some(item)) = (parts.next(), parts.next(), parts.next())
    else {
        return true;
    };
    let hit = of_kind(app, mi, "sidebar_section")
        .find(|c| s(c, "plugin_id") == plugin && s(c, "id") == section)
        .and_then(|c| {
            c["items"]
                .as_array()?
                .iter()
                .find(|i| s(i, "id") == item)
                .map(|i| (s(i, "on_select").to_string(), clean(s(i, "label"), 60)))
        });
    if let Some((action, label)) = hit.filter(|(a, _)| !a.is_empty()) {
        let plugin = plugin.to_string();
        crate::plugins::run_action(app, mi, &plugin, Some(&action), &label, "sidebar");
    }
    true
}

/// Is this sidebar group id a plugin row?
pub fn is_plugin_row(gid: &str) -> bool {
    gid.starts_with(SECTION) || gid.starts_with(ITEM)
}

/// Badge and title suffix a plugin set for `pane` (appended to its sidebar rows).
pub fn decorate(app: &App, mi: usize, pane: &str, segs: &mut Vec<(String, Style)>) {
    let t = &app.theme;
    for d in of_kind(app, mi, "pane_decoration").filter(|d| s(d, "pane") == pane) {
        let suffix = clean(s(d, "title_suffix"), 40);
        if !suffix.is_empty() {
            segs.push((format!(" {suffix}"), t.dim()));
        }
        let badge = clean(s(d, "badge"), 12);
        if !badge.is_empty() {
            let st = match s(d, "border_color") {
                "" => t.s(t.accent),
                c => style_for(app, c),
            };
            segs.push((format!(" [{badge}]"), st));
        }
    }
}

/// Border style override for a border cell next to a decorated pane of the focused machine.
pub fn border_style(app: &App, rects: &[(String, Rect)], x: u16, y: u16) -> Option<Style> {
    let decos: Vec<&Value> = of_kind(app, app.cur, "pane_decoration")
        .filter(|d| !s(d, "border_color").is_empty())
        .collect();
    if decos.is_empty() {
        return None;
    }
    for (pane, r) in rects {
        let near = x + 1 >= r.x && x <= r.x + r.w && y + 1 >= r.y && y <= r.y + r.h;
        if !near {
            continue;
        }
        if let Some(d) = decos.iter().find(|d| s(d, "pane") == pane.as_str()) {
            return Some(style_for(app, s(d, "border_color")));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_and_sanitize() {
        let v = json!({"contributions": [{"kind": "status_segment", "plugin_id": "a.b", "id": "x", "text": "ok\u{1b}[2J"}]});
        let c = parse(&v);
        assert_eq!(c.len(), 1);
        assert_eq!(clean(s(&c[0], "text"), 40), "ok[2J");
        assert!(is_plugin_row(&format!("{ITEM}a\u{1f}b\u{1f}c")));
        assert!(!is_plugin_row("g1"));
    }

    fn app_with(contrib: Value) -> crate::app::App {
        let (mut app, _rx) = crate::app::test_app(1);
        crate::plugins::on_reply(
            &mut app,
            0,
            crate::plugins::Reply::Ui,
            Ok(json!({"window_title": null, "agent_views": [], "contributions": contrib})),
        );
        app
    }

    #[test]
    fn status_segments_sort_by_priority_and_side() {
        let app = app_with(json!([
            {"kind": "status_segment", "plugin_id": "a.b", "id": "lo", "text": "low", "side": "right", "priority": 1},
            {"kind": "status_segment", "plugin_id": "a.b", "id": "hi", "text": "high", "side": "right", "priority": 9},
            {"kind": "status_segment", "plugin_id": "a.b", "id": "l", "text": "left", "side": "left"},
            {"kind": "status_segment", "plugin_id": "a.b", "id": "e", "text": "", "side": "left"}
        ]));
        let r: Vec<String> = segments(&app, "right").into_iter().map(|x| x.1).collect();
        assert_eq!(r, ["high", "low"]);
        let l = segments(&app, "left");
        assert_eq!(l.len(), 1, "empty text dropped");
        assert_eq!(l[0].0, "plugin:a.b:l");
    }

    #[test]
    fn sidebar_sections_collapse_and_decorations_append() {
        let mut app = app_with(json!([
            {"kind": "sidebar_section", "plugin_id": "a.b", "id": "ci", "title": "CI", "order": 0,
             "items": [{"id": "main", "label": "main ✓", "badge": "3", "state_color": "green", "on_select": null}]},
            {"kind": "pane_decoration", "plugin_id": "a.b", "id": "p1", "pane": "p1", "badge": "CI", "title_suffix": "(green)", "border_color": "green"}
        ]));
        let mut rows = vec![];
        push_sidebar(&app, &mut rows);
        assert_eq!(rows.len(), 2);
        let text: String = rows[1].segs.iter().map(|s| s.0.clone()).collect();
        assert!(text.contains("main ✓") && text.contains("[3]"), "{text}");
        let gid = rows[0].group.clone().unwrap().1;
        assert!(activate(&mut app, 0, &gid));
        let mut rows = vec![];
        push_sidebar(&app, &mut rows);
        assert_eq!(rows.len(), 1, "collapsed");
        let mut segs = vec![];
        decorate(&app, 0, "p1", &mut segs);
        let t: String = segs.iter().map(|s| s.0.clone()).collect();
        assert_eq!(t, " (green) [CI]");
        let rects = vec![(
            "p1".to_string(),
            Rect {
                x: 0,
                y: 0,
                w: 10,
                h: 5,
            },
        )];
        assert!(border_style(&app, &rects, 10, 2).is_some());
        assert!(border_style(&app, &rects, 30, 30).is_none());
    }
}
