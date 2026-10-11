//! The config error notice: a server whose `config.toml` does not load keeps the last config
//! that did (lane C, `config_api`). The TUI says so until the file loads again:
//!
//! - **Source**: `config.get` (its `load_error: {path, errors}`) after each connect, and again
//!   on the pushed events `session.config_rejected` (a new error) and
//!   `session.config_reloaded` (the file loads again: the notice goes).
//! - **Notice**: a toast when an error first shows, and a badge in the tab bar's right cluster
//!   (`⚠ config.toml error: last good config in use`) that stays until the error is fixed or
//!   the user looked at it.
//! - **Details**: a click on the badge, or the `config_error` action (palette "Config error"),
//!   opens a popup that names the file and each error and says that the last good config is in
//!   use. Opening it dismisses the badge; a different error shows it again.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::app::{App, Mode, Pending, Popup, RpcErr};

/// Errors the popup lists (it has room for a few).
const MAX_SHOWN: usize = 4;

/// One machine's config error.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Notice {
    /// The config file that does not load.
    pub path: String,
    /// Each error as `line 3: expected a string` (or just the message).
    pub errors: Vec<String>,
    /// The user looked at it (the badge hides until the error changes).
    pub dismissed: bool,
}

impl Notice {
    /// A `load_error` value (`{path, errors: [{line, col, message}]}`), `None` when it is null.
    pub fn from_load_error(v: &Value) -> Option<Notice> {
        if !v.is_object() {
            return None;
        }
        let path = v
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("config.toml")
            .to_string();
        Some(Notice {
            path,
            errors: errors_of(&v["errors"]),
            dismissed: false,
        })
    }

    /// The file's name for the badge (`config.toml`).
    pub fn file_name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }

    /// The popup text.
    pub fn body(&self, machine: &str) -> String {
        let mut s = format!("{} on {machine} does not load:\n\n", self.path);
        if self.errors.is_empty() {
            s.push_str("  (no details)\n");
        }
        for e in self.errors.iter().take(MAX_SHOWN) {
            s.push_str(&format!("  {e}\n"));
        }
        if self.errors.len() > MAX_SHOWN {
            s.push_str(&format!("  … {} more\n", self.errors.len() - MAX_SHOWN));
        }
        s.push_str("\nVibeke uses the last config that loaded.\n");
        s.push_str("Fix the file and save it: the change applies at once.\n");
        s.push_str("`vibeke doctor` shows the details.");
        s
    }
}

/// `ConfigDiagnostic`s (`{line, col, message}`) as text.
pub fn errors_of(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|e| {
                    let msg = crate::devices::clean(e.get("message")?.as_str()?);
                    let line = e.get("line").and_then(Value::as_u64).unwrap_or(0);
                    Some(if line > 0 {
                        format!("line {line}: {msg}")
                    } else {
                        msg
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Debug, Default)]
pub struct State {
    pub per: BTreeMap<usize, Notice>,
}

#[derive(Debug, Clone)]
pub struct Reply;

/// After (re)connecting: ask whether the config loads.
pub fn on_connected(app: &mut App, mi: usize) {
    fetch(app, mi);
}

fn fetch(app: &mut App, mi: usize) {
    app.command_on(
        mi,
        "config.get",
        json!({}),
        Pending::Ux(crate::ux::Reply::Config(Reply)),
    );
}

/// Pushed `session.config_rejected` / `session.config_reloaded`.
pub fn on_event(app: &mut App, mi: usize, kind: &str, v: &Value) {
    match kind {
        "session.config_reloaded" => clear(app, mi),
        "session.config_rejected" => {
            // The event carries the errors but not the path: show them now, then read the path.
            let errors = errors_of(&v["data"]["errors"]);
            let path = app
                .ux
                .config
                .per
                .get(&mi)
                .map_or_else(|| "config.toml".to_string(), |n| n.path.clone());
            set(
                app,
                mi,
                Notice {
                    path,
                    errors,
                    dismissed: false,
                },
            );
            fetch(app, mi);
        }
        _ => {}
    }
}

pub fn on_reply(app: &mut App, mi: usize, _r: Reply, res: Result<Value, RpcErr>) {
    // An older server without `load_error`, or an error: nothing to say.
    let Ok(v) = res else {
        return;
    };
    match Notice::from_load_error(&v["load_error"]) {
        Some(n) => set(app, mi, n),
        None if v.get("load_error").is_some() => clear(app, mi),
        None => {}
    }
}

/// A (maybe) new error: a toast when it is new or changed, and the badge again.
fn set(app: &mut App, mi: usize, mut n: Notice) {
    let old = app.ux.config.per.get(&mi);
    // The same errors again (the event first, then `config.get` with the path): no new toast.
    let same = old.is_some_and(|o| o.errors == n.errors);
    if same {
        // Keep what the user did with it (and a path learned earlier).
        let o = old.unwrap();
        n.dismissed = o.dismissed;
        if n.path == "config.toml" {
            n.path = o.path.clone();
        }
    } else {
        let first = n.errors.first().cloned().unwrap_or_default();
        let label = app.machines.get(mi).map_or("", |m| m.label.as_str());
        let on = if app.machines.len() > 1 {
            format!(" on {label}")
        } else {
            String::new()
        };
        app.toast(format!(
            "⚠ {}{on} has an error ({first}): the last good config is in use",
            n.file_name()
        ));
    }
    app.ux.config.per.insert(mi, n);
    app.dirty = true;
}

fn clear(app: &mut App, mi: usize) {
    if app.ux.config.per.remove(&mi).is_some() {
        app.toast("config.toml loads again");
        app.dirty = true;
    }
}

/// The machine whose notice the badge shows: the current machine first.
fn shown(app: &App) -> Option<(usize, &Notice)> {
    let st = &app.ux.config.per;
    st.get(&app.cur)
        .filter(|n| !n.dismissed)
        .map(|n| (app.cur, n))
        .or_else(|| {
            st.iter()
                .find(|(_, n)| !n.dismissed)
                .map(|(mi, n)| (*mi, n))
        })
}

/// The badge in the tab bar's right cluster.
pub fn badge(app: &App) -> Option<String> {
    let (mi, n) = shown(app)?;
    let on = if mi == app.cur {
        String::new()
    } else {
        format!(
            " on {}",
            app.machines.get(mi).map_or("", |m| m.label.as_str())
        )
    };
    Some(format!(
        " ⚠ {} error{on}: last good config in use ",
        n.file_name()
    ))
}

/// Open the details of the shown (else any) notice and dismiss its badge.
pub fn open(app: &mut App) -> bool {
    let mi = match shown(app) {
        Some((mi, _)) => mi,
        None => match app.ux.config.per.keys().next() {
            Some(mi) => *mi,
            None => {
                app.toast("config.toml loads without errors");
                return true;
            }
        },
    };
    let label = app
        .machines
        .get(mi)
        .map_or_else(String::new, |m| m.label.clone());
    let Some(n) = app.ux.config.per.get_mut(&mi) else {
        return true;
    };
    n.dismissed = true;
    let body = n.body(&label);
    app.mode = Mode::Popup(Popup::Message {
        title: "config error".into(),
        body,
    });
    app.dirty = true;
    true
}

/// `config_error`: the details popup.
pub fn action(app: &mut App, action: &str) -> bool {
    action == "config_error" && open(app)
}

/// A left click on the badge opens the details.
pub fn on_mouse(app: &mut App, me: &crate::event::MouseEvent) -> bool {
    use crate::event::{MouseButton, MouseEventKind};
    if !matches!(me.kind, MouseEventKind::Down(MouseButton::Left)) {
        return false;
    }
    let Some(b) = badge(app) else {
        return false;
    };
    if crate::draw::right_cluster_at(app, me.column, me.row).as_deref() != Some(b.as_str()) {
        return false;
    }
    open(app)
}

#[cfg(test)]
#[path = "config_notice_tests.rs"]
mod tests;
