//! Layout save/apply from the palette (07 §2.14, 08 §13): `layout_save` asks for a name,
//! exports the focused tab (`layout.export {format: toml}`) and — there is no config-write API
//! — shows the `[layouts.<name>]` snippet to paste into `config.toml` and copies it to the
//! clipboard. `layout_apply` lists `layout.list` and applies the chosen one in a new workspace
//! (`enter`) or into the focused workspace (`w`).

use crate::app::{App, Mode, Pending, Popup, Prompt, PromptKind, RpcErr};
use crate::parity::Reply;
use crate::screen::Grid;
use serde_json::{Value, json};
use vk_proto::input::{Key, KeyEvent, KeyKind, NamedKey};

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub name: String,
    /// `"2 tabs · 5 panes"`.
    pub summary: String,
    /// Why it can't be applied (parse/validation error).
    pub problem: Option<String>,
}

pub fn action(app: &mut App, action: &str) -> bool {
    let cur = app.cur;
    match action {
        "layout_save" => match app.focused_tab() {
            Some(t) => {
                app.mode = Mode::Prompt(Prompt {
                    kind: PromptKind::LayoutSave { mi: cur, tab: t.id },
                    label: "save layout as".into(),
                    input: t.title.unwrap_or_default(),
                })
            }
            None => app.toast("no focused tab"),
        },
        "layout_apply" => app.command("layout.list", json!({}), Pending::Parity(Reply::LayoutList)),
        _ => return false,
    }
    true
}

/// A layout name usable as a bare TOML key.
pub fn clean_name(s: &str) -> String {
    let n: String = s
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    n.trim_matches('-').to_string()
}

pub fn save(app: &mut App, mi: usize, tab: &str, name: &str) {
    let name = clean_name(name);
    if name.is_empty() {
        app.toast("layout name: letters, digits, - and _");
        return;
    }
    app.command_on(
        mi,
        "layout.export",
        json!({"tab": tab, "format": "toml"}),
        Pending::Parity(Reply::LayoutExport { name }),
    );
}

/// Re-root an exported layout document under `[layouts.<name>]`: table headers get the prefix,
/// top-level keys go under a `[layouts.<name>]` header (multi-line strings left alone).
pub fn config_snippet(name: &str, toml: &str) -> String {
    let prefix = format!("layouts.{name}");
    let mut top = vec![format!("[{prefix}]")];
    let mut rest = Vec::new();
    let mut in_tables = false;
    let mut in_multiline: Option<&str> = None;
    for line in toml.lines() {
        if let Some(delim) = in_multiline {
            if line.contains(delim) {
                in_multiline = None;
            }
            if in_tables { &mut rest } else { &mut top }.push(line.to_string());
            continue;
        }
        let t = line.trim_start();
        let out = if let Some(h) = t.strip_prefix("[[") {
            in_tables = true;
            format!("[[{prefix}.{h}")
        } else if let Some(h) = t.strip_prefix('[') {
            in_tables = true;
            format!("[{prefix}.{h}")
        } else {
            line.to_string()
        };
        for delim in ["\"\"\"", "'''"] {
            if t.matches(delim).count() % 2 == 1 {
                in_multiline = Some(delim);
            }
        }
        if in_tables { &mut rest } else { &mut top }.push(out);
    }
    let mut s = top.join("\n");
    if !rest.is_empty() {
        s.push('\n');
        s.push_str(&rest.join("\n"));
    }
    s.push('\n');
    s
}

pub fn on_export(app: &mut App, name: &str, res: Result<Value, RpcErr>) {
    let v = match res {
        Ok(v) => v,
        Err(e) => {
            app.toast(format!("✗ layout export: {}", e.message));
            return;
        }
    };
    let Some(toml) = v.get("toml").and_then(Value::as_str) else {
        app.toast("✗ layout export: the server returned no TOML");
        return;
    };
    let snippet = config_snippet(name, toml);
    app.set_clipboard(snippet.as_bytes(), false);
    let path = vk_config::config_path();
    app.mode = Mode::Popup(Popup::Message {
        title: format!("layout {name} — copied to the clipboard"),
        body: format!(
            "Paste into {} (no config-write API yet), then `vibeke layout apply {name}` or :layout_apply.\n\n{snippet}",
            path.display()
        ),
    });
}

pub fn parse_list(v: &Value) -> Vec<Entry> {
    v.get("layouts")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|l| {
                    let name = l
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let problem = l
                        .get("error")
                        .or_else(|| l.get("valid"))
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    let tabs = l.get("tabs").and_then(Value::as_u64).unwrap_or(0);
                    let panes = l.get("panes").and_then(Value::as_u64).unwrap_or(0);
                    Entry {
                        name,
                        summary: format!("{tabs} tab(s) · {panes} pane(s)"),
                        problem,
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn on_list(app: &mut App, mi: usize, res: Result<Value, RpcErr>) {
    match res {
        Ok(v) => {
            let layouts = parse_list(&v);
            if layouts.is_empty() {
                app.toast("no saved layouts — :layout_save, or [layouts.<name>] in config.toml");
                return;
            }
            app.mode = Mode::Popup(Popup::LayoutPick {
                mi,
                layouts,
                sel: 0,
            });
        }
        Err(e) => app.toast(format!("✗ layout list: {}", e.message)),
    }
}

pub fn pick_key(app: &mut App, ev: KeyEvent, mi: usize, layouts: Vec<Entry>, sel: usize) {
    let n = layouts.len().max(1);
    if ev.kind == KeyKind::Release {
        app.mode = Mode::Popup(Popup::LayoutPick { mi, layouts, sel });
        return;
    }
    let apply_here = matches!(ev.key, Key::Char('w'));
    match ev.key {
        Key::Named(NamedKey::Escape) | Key::Char('q') => {}
        Key::Named(NamedKey::Down) | Key::Char('j') => {
            app.mode = Mode::Popup(Popup::LayoutPick {
                mi,
                layouts,
                sel: (sel + 1) % n,
            })
        }
        Key::Named(NamedKey::Up) | Key::Char('k') => {
            app.mode = Mode::Popup(Popup::LayoutPick {
                mi,
                layouts,
                sel: (sel + n - 1) % n,
            })
        }
        Key::Named(NamedKey::Enter) | Key::Char('w') => {
            let Some(e) = layouts.get(sel).cloned() else {
                return;
            };
            if let Some(p) = &e.problem {
                app.toast(format!("layout {}: {p}", e.name));
                app.mode = Mode::Popup(Popup::LayoutPick { mi, layouts, sel });
                return;
            }
            let mut params = json!({"name": e.name, "focus": true});
            if apply_here && let Some(w) = app.focused_ws() {
                params["workspace"] = json!(w.id);
            }
            app.command_on(
                mi,
                "layout.apply",
                params,
                Pending::Parity(Reply::LayoutApplied { name: e.name }),
            );
        }
        _ => app.mode = Mode::Popup(Popup::LayoutPick { mi, layouts, sel }),
    }
}

pub fn draw_pick(app: &App, g: &mut Grid, layouts: &[Entry], sel: usize) {
    let t = app.theme;
    let h = (layouts.len() as u16 + 4).min(24);
    let mut b = crate::popups::frame(app, g, 64, h, "apply layout");
    for (i, e) in layouts.iter().enumerate() {
        let st = if i == sel {
            t.sel(t.fg)
        } else if e.problem.is_some() {
            t.dim()
        } else {
            t.text()
        };
        let note = match &e.problem {
            Some(p) => format!("✗ {}", crate::draw::truncate(p, 30)),
            None => e.summary.clone(),
        };
        b.line(&format!("{:<24} {note}", e.name), st);
    }
    b.line(
        "[enter] new workspace  [w] into this workspace  [esc]",
        t.dim(),
    );
}
