//! Plugin-provided agent views (`agent.view.set/clear`) and the default key bindings that
//! trusted, enabled plugins declare (`[[keys.command]]`), 07 §7.7.
//!
//! **Agent views.** A plugin invocation (identified by its broker, never by request fields) can
//! attach one short status line to an agent run: `agent.view.set {target, text, detail?,
//! tone?}`. The line (`text` <= 80 characters, controls and bidi overrides stripped; `detail`
//! <= 512 bytes for the peek; `tone` `info|ok|warn|error`) is shown next to the run in the
//! sidebar and in the peek. A plugin holds at most 16 views, one per run. A view belongs to
//! the grant it was set under: it disappears when the plugin is disabled, untrusted, unlinked,
//! uninstalled, re-reviewed (new grant) or its run is gone, and credentials are redacted.

use super::*;
use crate::core::Tx;

pub const MAX_TEXT_CHARS: usize = 80;
pub const MAX_DETAIL_BYTES: usize = 512;
pub const MAX_PER_PLUGIN: usize = 16;

#[derive(Debug, Clone, PartialEq)]
pub struct View {
    pub grant_id: String,
    pub text: String,
    pub detail: Option<String>,
    pub tone: String,
    pub updated_ms: i64,
}

/// `(plugin id, run id)` -> view.
pub type Views = HashMap<(String, String), View>;

/// Strip C0/C1 controls and bidi overrides, redact credentials, cut to `max` characters.
fn clean(s: &str, max: usize) -> String {
    let t: String = s
        .chars()
        .filter(|c| {
            !c.is_control()
                && !matches!(*c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}' | '\u{061C}')
        })
        .collect();
    vk_redact::redact(t.trim())
        .chars()
        .take(max)
        .collect::<String>()
        .trim()
        .to_string()
}

fn clean_detail(s: &str) -> String {
    let t: String = s
        .chars()
        .filter(|c| {
            (*c == '\n' || !c.is_control())
                && !matches!(*c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}' | '\u{061C}')
        })
        .collect();
    let r = vk_redact::redact(&t).into_owned();
    let mut end = r.len().min(MAX_DETAIL_BYTES);
    while !r.is_char_boundary(end) {
        end -= 1;
    }
    r[..end].to_string()
}

fn emit(server: &Server, kind: &str, data: Value) {
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.event(kind, json!({}), data);
    let _ = server.commit(&mut c, tx);
}

fn plugin_of(caller: &Caller) -> Result<(String, String), WireError> {
    match (&caller.plugin, &caller.grant_id) {
        (Some((id, _)), Some(g)) if !g.is_empty() => Ok((id.clone(), g.clone())),
        _ => Err(WireError::new(
            "permission_denied",
            "agent views can only be set by a trusted plugin invocation",
        )),
    }
}

/// `agent.view.set`.
pub fn set(server: &Server, caller: &Caller, sn: &Snap, p: &Value) -> Result<Value, WireError> {
    let (plugin, grant) = plugin_of(caller)?;
    let run = run_target(sn, p)?;
    let text = clean(req_s(p, "text")?, MAX_TEXT_CHARS);
    if text.is_empty() {
        return Err(WireError::new("invalid_params", "text must not be empty"));
    }
    let tone = match sp(p, "tone").unwrap_or("info") {
        t @ ("info" | "ok" | "warn" | "error") => t.to_string(),
        o => {
            return Err(WireError::new(
                "invalid_params",
                format!("tone `{o}` is not info, ok, warn or error"),
            ));
        }
    };
    let detail = sp(p, "detail").map(clean_detail).filter(|d| !d.is_empty());
    {
        let st = state(server);
        let mut v = st.views.lock().unwrap();
        let key = (plugin.clone(), run.id.clone());
        if !v.contains_key(&key)
            && v.keys().filter(|(pl, _)| *pl == plugin).count() >= MAX_PER_PLUGIN
        {
            return Err(WireError::new(
                "limit_exceeded",
                format!("a plugin can hold at most {MAX_PER_PLUGIN} agent views; clear one first"),
            ));
        }
        v.insert(
            key,
            View {
                grant_id: grant,
                text,
                detail,
                tone,
                updated_ms: now_ms(),
            },
        );
    }
    emit(
        server,
        "plugin.agent_view_changed",
        json!({"plugin": plugin, "run": run.id}),
    );
    Ok(ok())
}

/// `agent.view.clear`: one run's view, or every view of the plugin without a target.
pub fn clear(server: &Server, caller: &Caller, sn: &Snap, p: &Value) -> Result<Value, WireError> {
    let (plugin, _) = plugin_of(caller)?;
    let target = if sp(p, "target").or(sp(p, "agent_id")).is_some() {
        Some(run_target(sn, p)?.id)
    } else {
        None
    };
    let removed = {
        let st = state(server);
        let mut v = st.views.lock().unwrap();
        let before = v.len();
        v.retain(|(pl, run), _| !(*pl == plugin && target.as_ref().is_none_or(|t| t == run)));
        before - v.len()
    };
    if removed > 0 {
        emit(
            server,
            "plugin.agent_view_changed",
            json!({"plugin": plugin, "run": target, "cleared": true}),
        );
    }
    Ok(ok())
}

/// Drop views of plugins that are no longer active under the grant they were set with.
/// Returns how many were removed (an event is emitted when any were).
pub fn purge_inactive(server: &Server) -> usize {
    let reg = Registry::load(&plugin_dirs()).unwrap_or_default();
    let live = |plugin: &str, grant: &str| {
        reg.plugins.get(plugin).is_some_and(|e| {
            registry::entry_status(e).0 == Status::Active
                && e.trust.as_ref().is_some_and(|t| t.grant_id == grant)
        })
    };
    let removed = {
        let st = state(server);
        let mut v = st.views.lock().unwrap();
        let before = v.len();
        v.retain(|(pl, _), view| live(pl, &view.grant_id));
        before - v.len()
    };
    if removed > 0 {
        emit(
            server,
            "plugin.agent_view_changed",
            json!({"cleared": true, "reason": "plugin_inactive"}),
        );
    }
    removed
}

/// The registry changed: drop views of plugins that are no longer active and tell clients to
/// re-read actions, key bindings and views (`plugin.registry_changed`).
pub fn registry_changed(server: &Server) {
    purge_inactive(server);
    emit(server, "plugin.registry_changed", json!({}));
}

/// Views a client may show: plugin still active under the same grant, run still present.
pub fn list(server: &Server) -> Vec<Value> {
    let reg = Registry::load(&plugin_dirs()).unwrap_or_default();
    let sn = snap(server);
    let st = state(server);
    let v = st.views.lock().unwrap();
    let mut out: Vec<Value> = v
        .iter()
        .filter(|((pl, run), view)| {
            reg.plugins.get(pl).is_some_and(|e| {
                registry::entry_status(e).0 == Status::Active
                    && e.trust
                        .as_ref()
                        .is_some_and(|t| t.grant_id == view.grant_id)
            }) && sn.pane_run.values().any(|r| r.id == *run)
        })
        .map(|((pl, run), view)| {
            json!({
                "plugin_id": pl,
                "run": run,
                "text": view.text,
                "detail": view.detail,
                "tone": view.tone,
                "updated_ms": view.updated_ms,
            })
        })
        .collect();
    out.sort_by(|a, b| {
        (a["run"].as_str(), a["plugin_id"].as_str())
            .cmp(&(b["run"].as_str(), b["plugin_id"].as_str()))
    });
    out
}

/// Default `[[keys.command]]` bindings of every registered plugin. Only active (trusted and
/// enabled) plugins' bindings are `installed`; a binding that clashes with the user's keymap
/// (or an earlier plugin's) is skipped and reported with `conflicts_with`, and an unusable key
/// string with `reason: "invalid_key"`. User keys always win.
pub fn keybindings(reg: &Registry) -> Vec<Value> {
    let cfg = vk_config::Config::load(vk_config::config_path())
        .map(|(c, _)| c)
        .unwrap_or_default();
    let mut claimed: Vec<(String, String)> = Vec::new();
    let mut out = Vec::new();
    for e in reg.plugins.values() {
        let (st, m) = registry::entry_status(e);
        let Some(m) = m else { continue };
        for (key, action, description) in m.plugin_key_bindings() {
            let mut v = json!({
                "plugin_id": e.id,
                "key": key,
                "action": action,
                "description": description,
                "installed": false,
            });
            if st != Status::Active {
                v["reason"] = json!(st.as_str());
            } else {
                match vk_config::binding_clash(&cfg, &key, &claimed) {
                    Err(_) => v["reason"] = json!("invalid_key"),
                    Ok(Some(other)) => {
                        v["reason"] = json!("conflict");
                        v["conflicts_with"] = json!(other);
                    }
                    Ok(None) => {
                        v["installed"] = json!(true);
                        claimed.push((action.clone(), key.clone()));
                    }
                }
            }
            out.push(v);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_sanitized_and_bounded() {
        assert_eq!(clean("a\u{1b}[31mb\u{202e}c", 80), "a[31mbc");
        assert_eq!(
            clean(&"x".repeat(200), MAX_TEXT_CHARS).len(),
            MAX_TEXT_CHARS
        );
        let token = format!("ghp_{}", "A".repeat(36));
        assert!(!clean(&format!("tok {token}"), 80).contains(&token));
        let d = clean_detail(&format!("l1\nl2\u{7}\n{}", "é".repeat(600)));
        assert!(d.len() <= MAX_DETAIL_BYTES && d.starts_with("l1\nl2\n"));
    }
}
