//! Layout export/apply (07 §2.14, 08 §13 "Layout export/apply").
//!
//! `layout.export {tab | workspace, format?: json|toml}` turns live tabs into a declarative
//! [`LayoutSpec`] (splits with ratios, cwds relative to the workspace root, foreground commands
//! as `run`, floats). `layout.apply {layout | doc | name, workspace? | new_workspace?, cwd?,
//! focus?}` recreates it: tabs, splits, cwds, pane commands and typed `run` lines. Named layouts
//! live in config under `[layouts.<name>]` (`layout.list`); `workspace.create {layout}` and
//! `tab.create {layout}` apply one directly.

use crate::api::{Ctx, R, b, cursor, internal, invalid, not_found, resolve_tab, resolve_ws, s};
use crate::core::{Tx, ulid};
use crate::{Server, paths};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};
use vk_proto::layout_spec::{
    CommandSpec, FloatSpec, LayoutSpec, PaneSpec, RectPct, TabSpec, split_side_by_side,
};
use vk_proto::model::*;

/// Panes one apply may create.
pub const MAX_PANES: usize = 64;

const SHELLS: &[&str] = &[
    "sh", "bash", "zsh", "fish", "nu", "dash", "ksh", "tcsh", "csh", "elvish", "xonsh", "pwsh",
    "login",
];

fn is_shell(argv: &[String]) -> bool {
    argv.first().is_none_or(|a| {
        let base = a.rsplit('/').next().unwrap_or(a).trim_start_matches('-');
        SHELLS.contains(&base)
    })
}

/// A pane's root process as a layout command: `sh -c "…"` → the string, a non-shell argv as is,
/// an interactive shell → none.
fn command_of(argv: &[String]) -> Option<CommandSpec> {
    match argv {
        [] => None,
        [sh, c, cmd] if is_shell(std::slice::from_ref(sh)) && c == "-c" => {
            Some(CommandSpec::Shell(cmd.clone()))
        }
        _ if is_shell(argv) => None,
        _ => Some(CommandSpec::Argv(argv.to_vec())),
    }
}

fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if !a.is_empty()
                && a.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,".contains(c))
            {
                a.clone()
            } else {
                crate::notify::sh_quote(a)
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn rel_cwd(cwd: Option<&str>, root: &str) -> Option<String> {
    let canon = |p: &str| {
        std::fs::canonicalize(p)
            .map(|x| x.to_string_lossy().into_owned())
            .unwrap_or_else(|_| p.to_string())
    };
    let (cwd, root) = (canon(cwd?), canon(root));
    let (cwd, root) = (cwd.as_str(), root.as_str());
    if cwd == root {
        return None;
    }
    match cwd.strip_prefix(root).and_then(|r| r.strip_prefix('/')) {
        Some(rel) if !rel.is_empty() => Some(rel.to_string()),
        _ => Some(cwd.to_string()),
    }
}

fn leaf_spec(server: &Server, pane_id: &str, root: &str, focused: bool) -> PaneSpec {
    let (pane, run) = server.with_core(|c| {
        (
            c.pane(pane_id).cloned(),
            c.run_for_pane(pane_id).map(|r| r.harness.clone()),
        )
    });
    let Some(pane) = pane else {
        return PaneSpec::default();
    };
    let cwd = server.pane_cwd(pane_id).or(pane.cwd.clone());
    // A pane started with a command (not a shell) exports it as `command`; a command typed
    // into a shell (or an agent) as `run`.
    let root_argv = match (pane.child_pid, pane.isolation.is_contained()) {
        (Some(pid), false) => vk_hold::procinfo::argv(pid),
        _ => vec![],
    };
    let command = if run.is_some() {
        None
    } else {
        command_of(&root_argv)
    };
    let run = match command {
        Some(_) => None,
        None => run.or_else(|| (!is_shell(&pane.fg_cmdline)).then(|| shell_join(&pane.fg_cmdline))),
    };
    PaneSpec {
        cwd: rel_cwd(cwd.as_deref(), root),
        title: pane.title.clone(),
        command,
        run,
        focus: focused,
        ..Default::default()
    }
}

fn node_spec(server: &Server, n: &LayoutNode, root: &str, focused: Option<&str>) -> PaneSpec {
    match n {
        LayoutNode::Leaf { pane } => leaf_spec(server, pane, root, focused == Some(pane)),
        LayoutNode::Split { dir, children } => PaneSpec {
            split: Some(
                match dir {
                    SplitDir::Horizontal => "right",
                    SplitDir::Vertical => "down",
                }
                .into(),
            ),
            children: children
                .iter()
                .map(|(c, r)| {
                    let mut x = node_spec(server, c, root, focused);
                    x.size = Some((*r as f64 * 1000.0).round() / 1000.0);
                    x
                })
                .collect(),
            ..Default::default()
        },
    }
}

pub fn export_tab(server: &Server, tab: &Tab, root: &str, focus: bool) -> TabSpec {
    let floats = tab
        .floating
        .iter()
        .map(|f| {
            let leaf = leaf_spec(server, &f.pane, root, false);
            FloatSpec {
                cwd: leaf.cwd,
                run: leaf.run,
                title: leaf.title,
                command: leaf.command,
                rect: Some(RectPct {
                    x: f.x as f64,
                    y: f.y as f64,
                    w: f.w as f64,
                    h: f.h as f64,
                }),
            }
        })
        .collect();
    TabSpec {
        title: tab.title.clone(),
        cwd: None,
        focus,
        pane: node_spec(server, &tab.layout, root, tab.focused_pane.as_deref()),
        floats,
    }
}

fn expand(p: &str, base: Option<&str>) -> String {
    let home = paths::home().to_string_lossy().into_owned();
    if p == "~" {
        return home;
    }
    if let Some(rest) = p.strip_prefix("~/") {
        return format!("{home}/{rest}");
    }
    if p.starts_with('/') {
        return p.to_string();
    }
    match base {
        Some(b) if p == "." => b.to_string(),
        Some(b) => format!("{}/{}", b.trim_end_matches('/'), p.trim_start_matches("./")),
        None => p.to_string(),
    }
}

/// Parse a layout from `layout` (object), `doc` (TOML or JSON text) or `name` (config).
pub fn spec_from_params(p: &Value) -> Result<LayoutSpec, vk_proto::rpc::RpcError> {
    let spec = match (p.get("layout"), s(p, "doc"), s(p, "name")) {
        (Some(Value::Object(_)), _, _) => serde_json::from_value(p["layout"].clone())
            .map_err(|e| invalid(format!("layout: {e}")))?,
        (Some(Value::String(name)), _, _) => named(name)?,
        (_, Some(doc), _) => parse_doc(doc)?,
        // The CLI turns a JSON `--doc` into an object.
        (_, None, _) if p.get("doc").is_some_and(Value::is_object) => {
            parse_doc(&p["doc"].to_string())?
        }
        (_, _, Some(name)) => named(name)?,
        _ => {
            return Err(invalid(
                "layout (object), doc (TOML/JSON text) or name required",
            ));
        }
    };
    spec.validate(MAX_PANES).map_err(invalid)?;
    Ok(spec)
}

pub fn parse_doc(doc: &str) -> Result<LayoutSpec, vk_proto::rpc::RpcError> {
    let t = doc.trim_start();
    if t.starts_with('{') {
        let v: Value = serde_json::from_str(doc).map_err(|e| invalid(format!("json: {e}")))?;
        // An export result (`{"layout": {...}}`) applies as-is.
        let v = match v.get("layout") {
            Some(l) if l.is_object() => l.clone(),
            _ => v,
        };
        return serde_json::from_value(v).map_err(|e| invalid(format!("layout: {e}")));
    }
    toml::from_str(doc).map_err(|e| invalid(format!("toml: {e}")))
}

fn config_layouts() -> std::collections::BTreeMap<String, toml::Value> {
    vk_config::Config::load(vk_config::config_path())
        .map(|(c, _)| c.layouts)
        .unwrap_or_default()
}

pub fn named(name: &str) -> Result<LayoutSpec, vk_proto::rpc::RpcError> {
    let v = config_layouts()
        .remove(name)
        .ok_or_else(|| not_found("layout", name))?;
    let mut spec: LayoutSpec = v
        .try_into()
        .map_err(|e| invalid(format!("[layouts.{name}]: {e}")))?;
    spec.name.get_or_insert_with(|| name.to_string());
    Ok(spec)
}

/// The caller's cwd (`default_cwd`, sent by the CLI) when neither params nor the layout set one.
fn fallback_cwd(spec: &LayoutSpec, p: &Value) -> Option<String> {
    spec.cwd
        .is_none()
        .then(|| s(p, "default_cwd").map(str::to_string))
        .flatten()
}

pub enum Target {
    New {
        cwd: Option<String>,
        name: Option<String>,
    },
    Existing(Workspace),
}

struct Pending {
    pane: String,
    run: String,
}

/// Build a tab's tree, spawning one pane per leaf.
#[allow(clippy::too_many_arguments)]
fn build(
    server: &Arc<Server>,
    c: &mut crate::core::Core,
    tx: &mut Tx,
    ws: &Workspace,
    tab_id: &str,
    tab_handle: &str,
    node: &PaneSpec,
    base: &str,
    created_by: &str,
    pending: &mut Vec<Pending>,
    focus: &mut Option<String>,
) -> anyhow::Result<LayoutNode> {
    if node.is_split() {
        let side = split_side_by_side(node.split.as_deref().unwrap_or("right")).unwrap_or(true);
        let given: f64 = node.children.iter().filter_map(|c| c.size).sum();
        let unsized_n = node.children.iter().filter(|c| c.size.is_none()).count() as f64;
        // Unsized children share what's left (or an equal part when sizes don't fit in 1).
        let fill = if given < 1.0 && unsized_n > 0.0 {
            (1.0 - given) / unsized_n
        } else {
            1.0 / node.children.len() as f64
        };
        let mut children = Vec::new();
        for ch in &node.children {
            let sub = build(
                server, c, tx, ws, tab_id, tab_handle, ch, base, created_by, pending, focus,
            )?;
            children.push((sub, ch.size.unwrap_or(fill) as f32));
        }
        let sum: f32 = children.iter().map(|(_, r)| r).sum::<f32>().max(0.0001);
        for ch in children.iter_mut() {
            ch.1 /= sum;
        }
        return Ok(LayoutNode::Split {
            dir: if side {
                SplitDir::Horizontal
            } else {
                SplitDir::Vertical
            },
            children,
        });
    }
    let cwd = node
        .cwd
        .as_deref()
        .map(|x| expand(x, Some(base)))
        .unwrap_or_else(|| base.to_string());
    let pane = server.new_pane(
        c,
        tx,
        ws,
        tab_id,
        tab_handle,
        &cwd,
        node.command.as_ref().map(CommandSpec::argv),
        node.title.clone(),
        created_by,
    )?;
    if let Some(r) = node.run.as_ref().filter(|r| !r.trim().is_empty()) {
        pending.push(Pending {
            pane: pane.id.clone(),
            run: r.clone(),
        });
    }
    if node.focus || focus.is_none() {
        *focus = Some(pane.id.clone());
    }
    Ok(LayoutNode::Leaf { pane: pane.id })
}

/// Type `run` lines into the new shells once they've drawn something.
fn send_runs(server: &Arc<Server>, pending: Vec<Pending>) {
    if pending.is_empty() {
        return;
    }
    let srv = server.clone();
    tokio::spawn(async move {
        for p in pending {
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline && srv.pane_rt(&p.pane).is_none_or(|rt| rt.rev() == 0) {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
            let mut bytes = p.run.into_bytes();
            bytes.push(b'\r');
            let id = srv.next_internal_input_id();
            let _ = crate::render::write_and_ack(&srv, &p.pane, id, bytes).await;
        }
    });
}

pub fn apply(server: &Arc<Server>, ctx: &Ctx, spec: &LayoutSpec, target: Target, focus: bool) -> R {
    let created_by = match &ctx.pane_scope {
        Some(p) => format!("agent:{p}"),
        None => "user".to_string(),
    };
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    let (ws, new_ws) = match target {
        Target::Existing(w) => (w, false),
        Target::New { cwd, name } => {
            let root = cwd
                .map(|x| expand(&x, None))
                .or_else(|| spec.cwd.as_deref().map(|x| expand(x, None)))
                .unwrap_or_else(|| paths::home().to_string_lossy().into_owned());
            let auto = crate::autoname::auto_name(&root);
            let order = c
                .model
                .workspaces
                .iter()
                .map(|w| w.order)
                .fold(0.0, f64::max)
                + 1.0;
            let ws = Workspace {
                id: ulid(),
                handle: c.next_ws_handle(),
                name: name.or_else(|| spec.name.clone()),
                auto_name: auto,
                root_path: root.clone(),
                task: None,
                order,
                branch: None,
            };
            tx.ws(ws.clone());
            tx.event(
                "workspace.created",
                json!({"workspace": ws.id}),
                json!({"cwd": root, "layout": spec.name}),
            );
            (ws, true)
        }
    };
    let base_root = if new_ws {
        ws.root_path.clone()
    } else {
        spec.cwd
            .as_deref()
            .map(|x| expand(x, Some(&ws.root_path)))
            .unwrap_or_else(|| ws.root_path.clone())
    };
    let mut tabs = Vec::new();
    let mut pending = Vec::new();
    let mut focus_pane: Option<String> = None;
    let mut first_order = c
        .tabs_of(&ws.id)
        .iter()
        .map(|t| t.order)
        .fold(0.0, f64::max);
    for t in &spec.tabs {
        let base = t
            .cwd
            .as_deref()
            .map(|x| expand(x, Some(&base_root)))
            .unwrap_or_else(|| base_root.clone());
        let id = ulid();
        let number = c.next_tab_number(&ws.id);
        let handle = format!("{}:t{number}", ws.handle);
        let mut tab_focus = None;
        let layout = match build(
            server,
            &mut c,
            &mut tx,
            &ws,
            &id,
            &handle,
            &t.pane,
            &base,
            &created_by,
            &mut pending,
            &mut tab_focus,
        ) {
            Ok(l) => l,
            Err(e) => {
                // Holders already spawned for this apply are orphaned without a commit: close them.
                for p in &tx.panes {
                    if let Some(rt) = server.pane_rt(&p.id) {
                        rt.send(crate::pane::PaneCmd::Close);
                    }
                }
                return Err(internal(format!("{e:#}")));
            }
        };
        let mut floating = Vec::new();
        for (z, f) in t.floats.iter().enumerate() {
            let leaf = PaneSpec {
                command: f.command.clone(),
                run: f.run.clone(),
                cwd: f.cwd.clone(),
                title: f.title.clone(),
                ..Default::default()
            };
            let mut dummy = Some(String::new());
            let node = build(
                server,
                &mut c,
                &mut tx,
                &ws,
                &id,
                &handle,
                &leaf,
                &base,
                &created_by,
                &mut pending,
                &mut dummy,
            )
            .map_err(|e| internal(format!("{e:#}")))?;
            let LayoutNode::Leaf { pane } = node else {
                continue;
            };
            let r = f.rect.unwrap_or(RectPct {
                x: 15.0,
                y: 15.0,
                w: 70.0,
                h: 70.0,
            });
            floating.push(FloatingPane {
                pane,
                x: r.x.clamp(0.0, 100.0) as f32,
                y: r.y.clamp(0.0, 100.0) as f32,
                w: r.w.clamp(1.0, 100.0) as f32,
                h: r.h.clamp(1.0, 100.0) as f32,
                z: z as u32 + 1,
            });
        }
        first_order += 1.0;
        let tab = Tab {
            id: id.clone(),
            handle,
            workspace: ws.id.clone(),
            title: t.title.clone(),
            number,
            focused_pane: tab_focus.clone(),
            layout,
            zoomed_pane: None,
            order: first_order,
            floating,
            floats_hidden: false,
        };
        tx.event(
            "tab.created",
            json!({"tab": id, "workspace": ws.id}),
            json!({"number": number, "layout": spec.name}),
        );
        if t.focus || focus_pane.is_none() {
            focus_pane = tab_focus;
        }
        tx.tab(tab.clone());
        tabs.push(tab);
    }
    tx.event(
        "layout.applied",
        json!({"workspace": ws.id}),
        json!({"name": spec.name, "tabs": tabs.len(), "panes": tx.panes.len(), "new_workspace": new_ws}),
    );
    let panes = tx.panes.clone();
    server.commit(&mut c, tx).map_err(internal)?;
    drop(c);
    send_runs(server, pending);
    if focus && let Some(p) = &focus_pane {
        server.focus_pane(&ctx.client_id, p);
    }
    Ok(json!({"workspace": ws, "tabs": tabs, "panes": panes, "cursor": cursor(server, None)}))
}

pub const METHODS: &[(&str, bool)] = &[
    ("layout.export", false),
    ("layout.apply", true),
    ("layout.list", false),
    ("layout.get", false),
];

pub fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    Some(match method {
        "layout.export" => (|| {
            let (spec, tab_only) = match (s(p, "workspace"), s(p, "tab")) {
                (Some(w), None) => {
                    let ws = resolve_ws(server, ctx, Some(w))?;
                    let focused_tab = server.client_focus(&ctx.client_id).tab;
                    let tabs: Vec<Tab> =
                        server.with_core(|c| c.tabs_of(&ws.id).into_iter().cloned().collect());
                    let n = tabs.len();
                    let specs = tabs
                        .iter()
                        .map(|t| {
                            let f = focused_tab.as_deref() == Some(&t.id);
                            export_tab(server, t, &ws.root_path, f && n > 1)
                        })
                        .collect();
                    (
                        LayoutSpec {
                            name: Some(ws.display_name().to_string()),
                            description: None,
                            cwd: Some(ws.root_path.clone()),
                            tabs: specs,
                        },
                        false,
                    )
                }
                (_, t) => {
                    let tab = resolve_tab(server, ctx, t)?;
                    let root = server
                        .with_core(|c| c.ws(&tab.workspace).map(|w| w.root_path.clone()))
                        .unwrap_or_default();
                    (
                        LayoutSpec {
                            name: tab.title.clone(),
                            description: None,
                            cwd: Some(root.clone()),
                            tabs: vec![export_tab(server, &tab, &root, false)],
                        },
                        true,
                    )
                }
            };
            let mut out =
                json!({"layout": spec, "scope": if tab_only { "tab" } else { "workspace" }});
            if s(p, "format") == Some("toml") {
                out["toml"] = json!(toml::to_string_pretty(&spec).map_err(internal)?);
            }
            Ok(out)
        })(),
        "layout.list" => {
            let list: Vec<Value> = config_layouts()
                .into_iter()
                .map(|(name, v)| {
                    let parsed: Result<LayoutSpec, _> = v.try_into();
                    match parsed {
                        Ok(sp) => json!({"name": name, "description": sp.description, "cwd": sp.cwd,
                                         "tabs": sp.tabs.len(),
                                         "panes": sp.tabs.iter().map(|t| t.pane.leaves().len() + t.floats.len()).sum::<usize>(),
                                         "valid": sp.validate(MAX_PANES).err()}),
                        Err(e) => json!({"name": name, "error": e.to_string()}),
                    }
                })
                .collect();
            Ok(json!({"layouts": list}))
        }
        "layout.get" => match s(p, "name") {
            Some(n) => named(n).map(|sp| json!({"layout": sp})),
            None => Err(invalid("missing param `name`")),
        },
        "layout.apply" => (|| {
            let spec = spec_from_params(p)?;
            let target = match (s(p, "workspace"), p.get("new_workspace")) {
                (Some(w), _) => Target::Existing(resolve_ws(server, ctx, Some(w))?),
                (None, nw) => Target::New {
                    cwd: nw
                        .and_then(|v| v.get("cwd"))
                        .and_then(Value::as_str)
                        .or(s(p, "cwd"))
                        .map(str::to_string)
                        .or_else(|| fallback_cwd(&spec, p)),
                    name: nw
                        .and_then(|v| v.get("name"))
                        .and_then(Value::as_str)
                        .or(s(p, "ws_name"))
                        .map(str::to_string),
                },
            };
            apply(server, ctx, &spec, target, b(p, "focus").unwrap_or(false))
        })(),
        // `workspace.create {layout}` / `tab.create {layout}` (07 §2.4–2.5).
        "workspace.create" if p.get("layout").is_some_and(|l| !l.is_null()) => (|| {
            let spec = spec_from_params(p)?;
            apply(
                server,
                ctx,
                &spec,
                Target::New {
                    cwd: s(p, "cwd")
                        .map(str::to_string)
                        .or_else(|| fallback_cwd(&spec, p)),
                    name: s(p, "name").map(str::to_string),
                },
                b(p, "focus").unwrap_or(false),
            )
        })(),
        "tab.create" if p.get("layout").is_some_and(|l| !l.is_null()) => (|| {
            let spec = spec_from_params(p)?;
            let ws = resolve_ws(server, ctx, s(p, "workspace"))?;
            apply(
                server,
                ctx,
                &spec,
                Target::Existing(ws),
                b(p, "focus").unwrap_or(false),
            )
        })(),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert!(is_shell(&["-zsh".into()]));
        assert!(is_shell(&["/bin/bash".into(), "-l".into()]));
        assert!(is_shell(&[]));
        assert!(!is_shell(&["npm".into(), "run".into(), "dev".into()]));
        assert_eq!(
            shell_join(&["echo".into(), "a b".into(), "x=1".into()]),
            "echo 'a b' x=1"
        );
        assert_eq!(rel_cwd(Some("/r/src"), "/r").as_deref(), Some("src"));
        assert_eq!(rel_cwd(Some("/r"), "/r"), None);
        assert_eq!(rel_cwd(Some("/other"), "/r").as_deref(), Some("/other"));
        assert_eq!(expand("src", Some("/r")), "/r/src");
        assert_eq!(expand("./a", Some("/r/")), "/r/a");
        assert_eq!(expand("/abs", Some("/r")), "/abs");
        assert_eq!(expand(".", Some("/r")), "/r");
    }

    #[test]
    fn toml_doc_parses() {
        let spec = parse_doc(
            r#"
name = "dev"
[[tab]]
title = "edit"
[tab.pane]
split = "right"
[[tab.pane.children]]
size = 0.7
run = "nvim"
[[tab.pane.children]]
command = ["htop"]
[[tab.float]]
command = "lazygit"
rect = { x = 10, y = 10, w = 80, h = 80 }
"#,
        )
        .unwrap();
        spec.validate(MAX_PANES).unwrap();
        assert_eq!(spec.tabs[0].pane.children.len(), 2);
        assert_eq!(spec.tabs[0].floats.len(), 1);
        let json_doc = serde_json::to_string(&json!({"layout": spec})).unwrap();
        assert_eq!(parse_doc(&json_doc).unwrap(), spec);
        assert!(parse_doc("[[tab]]\nbogus = 1").is_err());
    }
}
