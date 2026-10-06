//! Repo-local config in the TUI (08 §11.1, 09 §4).
//!
//! When a workspace is focused for the first time in this client, its root is checked with
//! `policy.trust {path, check: true}` (read-only). A repo with an untrusted (or changed)
//! `.vibeke/config.toml` gets a one-time notice "… :trust_repo to review"; nothing from the file
//! applies until then. `:trust_repo` shows the file, what is ignored (sections a repo may not set,
//! `allow` rules), the commands it adds and the `.vibeke/` digest; `y` records trust with
//! `policy.trust {path, digest}` — the digest the user reviewed, so a file edited in between is
//! refused. Once trusted, the repo's `[[keys.command]]` entries appear in the palette as
//! `Repo command: <title>` and their keys work while a workspace of that repo is focused;
//! `[tasks]` keys apply to new tasks (server side).

use crate::app::{App, Mode, Pending, Popup, RpcErr};
use crate::screen::Grid;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use vk_proto::input::{Key, KeyEvent, KeyKind, NamedKey};

/// What `policy.trust {check: true}` said about one workspace's repo.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Info {
    pub repo: String,
    pub file: Option<String>,
    pub digest: Option<String>,
    pub trusted: bool,
    pub text: Option<String>,
    pub warnings: Vec<String>,
    pub commands: Vec<vk_config::KeyCommand>,
    pub error: Option<String>,
    /// A trusted repo's `[preview]` layered over this client's config (08 §11.1); `None` =
    /// untrusted, no `[preview]` in the file, or invalid (the user's config applies).
    pub preview: Option<vk_config::Preview>,
}

impl Info {
    pub fn from_value(v: &Value) -> Info {
        let st = |k: &str| v[k].as_str().map(str::to_string);
        Info {
            repo: st("repo").unwrap_or_default(),
            file: st("file"),
            digest: st("digest"),
            trusted: v["trusted"].as_bool().unwrap_or(false),
            text: st("text"),
            warnings: v["warnings"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            commands: v["commands"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|c| {
                            let mut c = c.clone();
                            if let Some(o) = c.as_object_mut() {
                                o.retain(|_, x| !x.is_null());
                            }
                            serde_json::from_value::<vk_config::KeyCommand>(c).ok()
                        })
                        .collect()
                })
                .unwrap_or_default(),
            error: st("error"),
            preview: None,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct State {
    /// (machine, workspace) → repo info.
    pub info: HashMap<(usize, String), Info>,
    /// Checks sent (once per workspace per client run).
    pub asked: HashSet<(usize, String)>,
    /// Workspaces whose untrusted notice was shown.
    pub noticed: HashSet<(usize, String)>,
    /// Bindings installed: (machine, workspace).
    pub bound: HashSet<(usize, String)>,
    /// The review view: (machine, workspace).
    pub view: Option<(usize, String)>,
    pub notice: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Reply {
    Check {
        ws: String,
        open: bool,
    },
    Trusted {
        ws: String,
    },
    /// The fresh trust check before running a repo command: run `command` only when the repo
    /// is still trusted at the reviewed `digest` and still defines exactly that command.
    Run {
        ws: String,
        command: Box<vk_config::KeyCommand>,
        digest: Option<String>,
    },
}

fn focused(app: &App) -> Option<(usize, String, String)> {
    let w = app.focused_ws()?;
    Some((app.cur, w.id, w.root_path))
}

fn check(app: &mut App, mi: usize, ws: &str, root: &str, open: bool) {
    app.ux.trust.asked.insert((mi, ws.to_string()));
    app.command_on(
        mi,
        "policy.trust",
        json!({"path": root, "check": true}),
        Pending::Ux(crate::ux::Reply::Trust(Reply::Check {
            ws: ws.to_string(),
            open,
        })),
    );
}

/// Check the focused workspace's repo once.
pub fn tick(app: &mut App) {
    let Some((mi, ws, root)) = focused(app) else {
        return;
    };
    if root.is_empty()
        || !app.machines[mi].connected()
        || app.ux.trust.asked.contains(&(mi, ws.clone()))
    {
        return;
    }
    check(app, mi, &ws, &root, false);
}

pub fn on_reply(app: &mut App, mi: usize, r: Reply, res: Result<Value, RpcErr>) {
    match r {
        Reply::Check { ws, open } => {
            let mut info = match res {
                Ok(v) => Info::from_value(&v),
                Err(e) => {
                    if open {
                        app.toast(format!("✗ can't check this repo: {}", e.message));
                    }
                    return;
                }
            };
            let key = (mi, ws.clone());
            if info.file.is_some() && info.trusted {
                bind(app, mi, &ws, &info);
                info.preview = crate::repo_preview::layered(app, &info);
            }
            if info.file.is_some()
                && !info.trusted
                && !open
                && app.ux.trust.noticed.insert(key.clone())
            {
                app.toast(
                    "this repo has .vibeke/config.toml (not trusted) — :trust_repo to review",
                );
            }
            app.ux.trust.info.insert(key.clone(), info.clone());
            if open {
                if info.file.is_none() {
                    app.toast(format!("{} has no .vibeke/config.toml", info.repo));
                    return;
                }
                app.ux.trust.view = Some(key);
                app.ux.trust.notice = None;
                app.mode = Mode::Popup(Popup::TrustRepo);
            }
        }
        Reply::Run {
            ws,
            command,
            digest,
        } => {
            let info = match res {
                Ok(v) => Info::from_value(&v),
                Err(e) => {
                    app.toast(format!(
                        "✗ repo command not run: can't re-check trust ({})",
                        e.message
                    ));
                    app.dirty = true;
                    return;
                }
            };
            let still = info.trusted
                && info.digest.is_some()
                && info.digest == digest
                && info.commands.contains(&command);
            let key = (mi, ws);
            if still {
                app.ux.trust.info.insert(key, info);
                app.run_key_command(&command);
            } else {
                // Changed since it was reviewed: nothing runs until the user trusts the new
                // content (any change needs a new review).
                let mut info = info;
                info.trusted = false;
                app.ux.trust.info.insert(key.clone(), info);
                app.toast(
                    "✗ repo config changed since you trusted it — review it again (:trust_repo)",
                );
                app.ux.trust.view = Some(key);
                app.ux.trust.notice =
                    Some(".vibeke/ changed since it was trusted; the command did not run".into());
                app.mode = Mode::Popup(Popup::TrustRepo);
            }
        }
        Reply::Trusted { ws } => match res {
            Ok(_) => {
                app.ux.trust.notice = Some("trusted — repo settings apply now".into());
                let root = app.machines[mi]
                    .model
                    .workspaces
                    .iter()
                    .find(|w| w.id == ws)
                    .map(|w| w.root_path.clone())
                    .unwrap_or_default();
                check(app, mi, &ws, &root, false);
            }
            Err(e) => app.ux.trust.notice = Some(format!("✗ not trusted: {}", e.message)),
        },
    }
    app.dirty = true;
}

/// Bind a trusted repo's command keys (once; user and default bindings win).
fn bind(app: &mut App, mi: usize, ws: &str, info: &Info) {
    if !app.ux.trust.bound.insert((mi, ws.to_string())) {
        return;
    }
    for (i, c) in info.commands.iter().enumerate() {
        if !c.key.is_empty() {
            app.keymap
                .add_plugin_binding(&format!("repo:{mi}:{ws}:{i}"), &c.key);
        }
    }
}

/// Palette entries for trusted repo commands: (id, description, binding).
pub fn palette_entries(app: &App) -> Vec<(String, String, Option<String>)> {
    let Some((mi, ws, _)) = focused(app) else {
        return vec![];
    };
    let Some(info) = app.ux.trust.info.get(&(mi, ws.clone())) else {
        return vec![];
    };
    if !info.trusted {
        return vec![];
    }
    info.commands
        .iter()
        .enumerate()
        .map(|(i, c)| {
            (
                format!("repo:{mi}:{ws}:{i}"),
                format!(
                    "Repo command: {}",
                    c.title.clone().unwrap_or_else(|| c.command.clone())
                ),
                (!c.key.is_empty()).then(|| c.key.clone()),
            )
        })
        .collect()
}

pub fn action(app: &mut App, action: &str) -> bool {
    if action == "trust_repo" {
        match focused(app) {
            Some((mi, ws, root)) => check(app, mi, &ws, &root, true),
            None => app.toast("no focused workspace"),
        }
        return true;
    }
    if let Some(rest) = action.strip_prefix("repo:") {
        let mut it = rest.splitn(3, ':');
        let (mi, ws, i) = (
            it.next().and_then(|x| x.parse::<usize>().ok()),
            it.next().map(str::to_string),
            it.next().and_then(|x| x.parse::<usize>().ok()),
        );
        let (Some(mi), Some(ws), Some(i)) = (mi, ws, i) else {
            return true;
        };
        if focused(app).map(|f| (f.0, f.1)) != Some((mi, ws.clone())) {
            app.toast("that repo command works in its own workspace");
            return true;
        }
        let cached = app
            .ux
            .trust
            .info
            .get(&(mi, ws.clone()))
            .filter(|info| info.trusted)
            .and_then(|info| Some((info.commands.get(i).cloned()?, info.digest.clone())));
        let Some((c, digest)) = cached else {
            app.toast("repo config is not trusted — :trust_repo");
            return true;
        };
        // Re-check trust with the server right before running a repo-provided command (09 §4):
        // the `.vibeke/` tree may have changed since it was reviewed (a checkout, an agent).
        let root = focused(app).map(|f| f.2).unwrap_or_default();
        app.command_on(
            mi,
            "policy.trust",
            json!({"path": root, "check": true}),
            Pending::Ux(crate::ux::Reply::Trust(Reply::Run {
                ws,
                command: Box::new(c),
                digest,
            })),
        );
        return true;
    }
    false
}

pub fn key(app: &mut App, ev: KeyEvent) {
    let keep = |app: &mut App| app.mode = Mode::Popup(Popup::TrustRepo);
    if ev.kind == KeyKind::Release {
        return keep(app);
    }
    let Some((mi, ws)) = app.ux.trust.view.clone() else {
        return;
    };
    match ev.key {
        Key::Named(NamedKey::Escape) | Key::Char('q' | 'n') => {
            app.ux.trust.view = None;
        }
        Key::Char('y') => {
            let info = app.ux.trust.info.get(&(mi, ws.clone())).cloned();
            match info {
                Some(i) if !i.trusted => {
                    app.command_on(
                        mi,
                        "policy.trust",
                        json!({"path": i.repo, "digest": i.digest}),
                        Pending::Ux(crate::ux::Reply::Trust(Reply::Trusted { ws })),
                    );
                    app.ux.trust.notice = Some("recording trust…".into());
                }
                _ => app.ux.trust.notice = Some("already trusted".into()),
            }
            keep(app);
        }
        _ => keep(app),
    }
}

pub fn draw(app: &App, g: &mut Grid) {
    let Some(key) = &app.ux.trust.view else {
        return;
    };
    let Some(info) = app.ux.trust.info.get(key) else {
        return;
    };
    let t = app.theme;
    let mut a = crate::drafts::Area::open(
        app,
        g,
        &format!("repo config · {}", info.file.clone().unwrap_or_default()),
    );
    a.line(
        if info.trusted {
            "✓ trusted (this exact .vibeke/ content)"
        } else {
            "not trusted: nothing from this file applies until you trust it"
        },
        if info.trusted {
            t.s(t.green)
        } else {
            t.bold(t.yellow)
        },
    );
    a.line(
        &format!(
            "digest of .vibeke/: {}",
            info.digest.as_deref().unwrap_or("-")
        ),
        t.dim(),
    );
    if let Some(e) = &info.error {
        a.line(&format!("✗ {e}"), t.s(t.red));
    }
    for w in &info.warnings {
        a.line(&format!("! {w}"), t.s(t.yellow));
    }
    if !info.commands.is_empty() {
        a.line("commands it adds:", t.bold(t.fg));
        for c in &info.commands {
            a.line(
                &format!(
                    "  {} {:<8} {}",
                    if c.key.is_empty() { "-" } else { &c.key },
                    c.kind.as_str(),
                    c.command
                ),
                t.text(),
            );
        }
    }
    a.line("── file ──", t.dim());
    for l in info.text.as_deref().unwrap_or("").lines() {
        a.line(l, t.text());
    }
    if let Some(n) = &app.ux.trust.notice {
        a.line("", t.text());
        a.line(n, t.bold(t.yellow));
    }
    a.footer(
        "[y] trust this content (any change needs a new review)   [esc] close",
        t.dim(),
    );
}

#[cfg(test)]
#[path = "trust_tests.rs"]
mod tests;
