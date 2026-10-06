//! UI contributions (07 §7.4): data only, rendered by clients (no plugin-drawn escape codes in
//! chrome; every string is sanitized: C0/C1 controls stripped, bidi overrides removed, length
//! capped, 09 §6).
//!
//! `ui.contribute {contributions: [{kind, …}], replace?: bool, remove?: [{kind, id}]}` (also
//! the `contributions` a process returns from `plugin.initialize`). Each kind needs its `ui`
//! capability (`palette_command` → `palette`, `keybinding` → `keybindings`):
//!
//! | kind | shape (id defaults) |
//! |---|---|
//! | `sidebar_section` | `{id, title, items: [{id, label, badge?, state_color?, on_select?}], order?}` |
//! | `status_segment` | `{id, text, color?, side: left\|right, priority?, on_click?}` |
//! | `pane` | `{id, title, command: argv}` — opened with `ui.pane.open` (palette entry) |
//! | `palette_command` | `{id, title, contexts?, action}` |
//! | `keybinding` | `{key, action}` (id = key) |
//! | `pane_decoration` | `{pane, badge?, border_color?, title_suffix?}` (id = pane) |
//! | `link_handler` | `{id, scheme? \| regex?, action, title?}` |
//! | `harness` | `{id, path}` — a harness manifest inside the plugin dir (04), loaded like a trusted repo manifest |
//!
//! `action`, `on_select` and `on_click` name an action of the same plugin's manifest. Changes
//! are announced with `ui.contributions_changed {plugin}`, at most 10 per second per plugin
//! (a burst ends with one trailing event). Contributions disappear when the plugin's process
//! stops, the plugin is disabled or its consent changes.

use super::{emit, state};
use crate::Server;
use crate::api::{Ctx, R, err, invalid};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use vk_compat::native::caps::{Capabilities, ui_cap_for};
use vk_proto::rpc::{ErrorKind, RpcError};

/// Updates per plugin are announced at most this often (07 §7.4: 10 Hz).
pub const DEBOUNCE: Duration = Duration::from_millis(100);
const MAX_PER_PLUGIN: usize = 200;
const MAX_ITEMS: usize = 50;

#[derive(Default)]
pub struct PluginUi {
    pub consent_id: String,
    /// (kind, id) → sanitized contribution.
    pub items: BTreeMap<(String, String), Value>,
    pub last_emit: Option<Instant>,
    pub scheduled: bool,
    pub harness_root: Option<PathBuf>,
}

#[derive(Default)]
pub struct Contribs {
    pub by_plugin: BTreeMap<String, PluginUi>,
}

/// Plugin text for chrome: no controls, no bidi overrides, at most `max` characters.
pub fn clean(s: &str, max: usize) -> String {
    s.chars()
        .filter(|c| {
            !c.is_control()
                && !matches!(*c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}' | '\u{061C}')
        })
        .take(max)
        .collect::<String>()
        .trim()
        .to_string()
}

fn color_ok(c: &str) -> bool {
    let named = [
        "red", "yellow", "green", "blue", "accent", "muted", "fg", "cyan", "magenta",
    ];
    named.contains(&c)
        || (c.len() == 7 && c.starts_with('#') && c[1..].chars().all(|x| x.is_ascii_hexdigit()))
        || c.parse::<u8>().is_ok()
}

fn str_field(o: &Map<String, Value>, k: &str, max: usize, required: bool) -> Result<Option<String>, String> {
    match o.get(k) {
        Some(Value::String(s)) => {
            let c = clean(s, max);
            if required && c.is_empty() {
                Err(format!("`{k}` is empty"))
            } else {
                Ok(Some(c))
            }
        }
        None | Some(Value::Null) if !required => Ok(None),
        None | Some(Value::Null) => Err(format!("`{k}` is required")),
        Some(_) => Err(format!("`{k}` must be a string")),
    }
}

fn id_ok(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '/'))
}

/// Validate and sanitize one contribution: `(kind, id, value)`.
pub fn validate(
    plugin: &str,
    actions: &[String],
    root: &Path,
    raw: &Value,
) -> Result<(String, String, Value), String> {
    let o = raw.as_object().ok_or("a contribution is an object")?;
    let kind = o
        .get("kind")
        .and_then(Value::as_str)
        .ok_or("`kind` is required")?
        .to_string();
    let action_ok = |k: &str| -> Result<Option<String>, String> {
        let Some(a) = o.get(k) else { return Ok(None) };
        let a = a.as_str().ok_or(format!("`{k}` must be an action id"))?;
        let a = a.strip_prefix(&format!("{plugin}.")).unwrap_or(a);
        if actions.iter().any(|x| x == a) {
            Ok(Some(a.to_string()))
        } else {
            Err(format!("`{k}`: {plugin} has no action `{a}`"))
        }
    };
    let color = |k: &str| -> Result<Option<String>, String> {
        match o.get(k).and_then(Value::as_str) {
            None => Ok(None),
            Some(c) if color_ok(c) => Ok(Some(c.to_string())),
            Some(c) => Err(format!("`{k}`: unknown color `{c}`")),
        }
    };
    let id_of = |k: &str| -> Result<String, String> {
        let id = o
            .get(k)
            .and_then(Value::as_str)
            .ok_or(format!("`{k}` is required"))?;
        if id_ok(id) {
            Ok(id.to_string())
        } else {
            Err(format!("`{k}` `{id}`: letters, digits, `-_.:/`, ≤ 64"))
        }
    };
    let (id, v) = match kind.as_str() {
        "sidebar_section" => {
            let id = id_of("id")?;
            let items = o
                .get("items")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if items.len() > MAX_ITEMS {
                return Err(format!("at most {MAX_ITEMS} items per section"));
            }
            let mut out = vec![];
            for it in &items {
                let io = it.as_object().ok_or("an item is an object")?;
                let iid = io
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|i| id_ok(i))
                    .ok_or("item `id` is required")?;
                let label = str_field(io, "label", 80, true)?;
                let badge = str_field(io, "badge", 12, false)?;
                let sc = match io.get("state_color").and_then(Value::as_str) {
                    Some(c) if color_ok(c) => Some(c.to_string()),
                    Some(c) => return Err(format!("item {iid}: unknown color `{c}`")),
                    None => None,
                };
                let sel = match io.get("on_select").and_then(Value::as_str) {
                    None => None,
                    Some(a) => {
                        let a = a.strip_prefix(&format!("{plugin}.")).unwrap_or(a);
                        if !actions.iter().any(|x| x == a) {
                            return Err(format!("item {iid}: {plugin} has no action `{a}`"));
                        }
                        Some(a.to_string())
                    }
                };
                out.push(json!({"id": iid, "label": label, "badge": badge, "state_color": sc, "on_select": sel}));
            }
            (
                id.clone(),
                json!({
                    "id": id,
                    "title": str_field(o, "title", 40, true)?,
                    "items": out,
                    "order": o.get("order").and_then(Value::as_i64).unwrap_or(0),
                }),
            )
        }
        "status_segment" => {
            let id = id_of("id")?;
            let side = match o.get("side").and_then(Value::as_str).unwrap_or("right") {
                s @ ("left" | "right") => s.to_string(),
                s => return Err(format!("`side` `{s}`: left or right")),
            };
            (
                id.clone(),
                json!({
                    "id": id,
                    "text": str_field(o, "text", 40, true)?,
                    "color": color("color")?,
                    "side": side,
                    "priority": o.get("priority").and_then(Value::as_i64).unwrap_or(0),
                    "on_click": action_ok("on_click")?,
                }),
            )
        }
        "pane" => {
            let id = id_of("id")?;
            let cmd: Vec<String> = o
                .get("command")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            if cmd.is_empty() {
                return Err("`command` (argv) is required".into());
            }
            (
                id.clone(),
                json!({"id": id, "title": str_field(o, "title", 60, true)?, "command": cmd}),
            )
        }
        "palette_command" => {
            let id = id_of("id")?;
            let contexts: Vec<String> = o
                .get("contexts")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                .unwrap_or_else(|| vec!["global".into()]);
            if let Some(c) = contexts
                .iter()
                .find(|c| !vk_compat::herdr::manifest::CONTEXTS.contains(&c.as_str()))
            {
                return Err(format!("unknown context `{c}`"));
            }
            let action = action_ok("action")?.ok_or("`action` is required")?;
            (
                id.clone(),
                json!({"id": id, "title": str_field(o, "title", 80, true)?, "contexts": contexts, "action": action}),
            )
        }
        "keybinding" => {
            let key = str_field(o, "key", 40, true)?.unwrap_or_default();
            let action = action_ok("action")?.ok_or("`action` is required")?;
            (key.clone(), json!({"id": key, "key": key, "action": action}))
        }
        "pane_decoration" => {
            let pane = id_of("pane")?;
            (
                pane.clone(),
                json!({
                    "id": pane,
                    "pane": pane,
                    "badge": str_field(o, "badge", 12, false)?,
                    "border_color": color("border_color")?,
                    "title_suffix": str_field(o, "title_suffix", 40, false)?,
                }),
            )
        }
        "link_handler" => {
            let id = id_of("id")?;
            let pattern = match (o.get("regex").and_then(Value::as_str), o.get("scheme").and_then(Value::as_str)) {
                (Some(r), _) => {
                    regex::RegexBuilder::new(r)
                        .size_limit(1 << 20)
                        .build()
                        .map_err(|e| format!("`regex`: {e}"))?;
                    r.to_string()
                }
                (None, Some(s)) if s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) => {
                    format!("^{}:", regex::escape(s))
                }
                (None, Some(s)) => return Err(format!("`scheme` `{s}` is not a URL scheme")),
                (None, None) => return Err("`scheme` or `regex` is required".into()),
            };
            let action = action_ok("action")?.ok_or("`action` is required")?;
            (
                id.clone(),
                json!({"id": id, "pattern": pattern, "action": action, "title": str_field(o, "title", 60, false)?}),
            )
        }
        "harness" => {
            let id = id_of("id")?;
            let rel = o
                .get("path")
                .and_then(Value::as_str)
                .ok_or("`path` is required")?;
            if rel.starts_with('/') || rel.split('/').any(|c| c == "..") || !rel.ends_with(".toml") {
                return Err("`path`: a .toml file inside the plugin directory".into());
            }
            let full = root.join(rel);
            let canon_root = std::fs::canonicalize(root).map_err(|e| e.to_string())?;
            let canon = std::fs::canonicalize(&full).map_err(|e| format!("`path`: {e}"))?;
            if !canon.starts_with(&canon_root) {
                return Err("`path` resolves outside the plugin directory".into());
            }
            (id.clone(), json!({"id": id, "path": rel}))
        }
        other => return Err(format!("unknown contribution kind `{other}`")),
    };
    let mut v = v;
    v["kind"] = json!(kind);
    v["plugin_id"] = json!(plugin);
    Ok((kind, id, v))
}

/// Apply contributions for `plugin` (caller checked identity and consent).
pub fn contribute(
    server: &Arc<Server>,
    plugin: &str,
    caps: &Capabilities,
    consent_id: &str,
    list: &[Value],
    replace: bool,
    remove: &[(String, String)],
) -> Result<Value, RpcError> {
    let (e, m) = super::active(server, plugin)?;
    let actions: Vec<String> = m
        .actions_on(vk_compat::herdr::current_platform())
        .iter()
        .map(|a| a.id.clone())
        .collect();
    let mut parsed = vec![];
    for (i, raw) in list.iter().enumerate() {
        let (kind, id, v) = validate(plugin, &actions, &e.root, raw)
            .map_err(|why| invalid(format!("contributions[{i}]: {why}")))?;
        if !caps.allows_ui(&kind) {
            let need = ui_cap_for(&kind).unwrap_or("?");
            return Err(err(
                ErrorKind::PermissionDenied,
                format!("capability_violation: contributions[{i}] ({kind}) needs ui = [\"{need}\"]"),
            ));
        }
        parsed.push((kind, id, v));
    }
    let (count, harness_changed) = {
        let st = state(server);
        let mut c = st.ui.lock().unwrap();
        let pu = c.by_plugin.entry(plugin.to_string()).or_default();
        if pu.consent_id != consent_id {
            pu.items.clear();
            pu.consent_id = consent_id.to_string();
        }
        let had_harness = pu.items.keys().any(|(k, _)| k == "harness");
        if replace {
            pu.items.clear();
        }
        for k in remove {
            pu.items.remove(k);
        }
        for (kind, id, v) in parsed {
            pu.items.insert((kind, id), v);
        }
        if pu.items.len() > MAX_PER_PLUGIN {
            return Err(err(
                ErrorKind::Conflict,
                format!("a plugin may hold at most {MAX_PER_PLUGIN} contributions"),
            ));
        }
        let has_harness = pu.items.keys().any(|(k, _)| k == "harness");
        (pu.items.len(), had_harness || has_harness)
    };
    if harness_changed {
        sync_harness(server, plugin, &e.root);
    }
    changed(server, plugin);
    Ok(json!({"plugin": plugin, "contributions": count}))
}

/// `ui.contribute` from a plugin connection.
pub fn api_contribute(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let info = super::require_plugin(server, ctx, "ui.contribute")?;
    let list = p
        .get("contributions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let remove: Vec<(String, String)> = p
        .get("remove")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| {
                    Some((
                        x.get("kind")?.as_str()?.to_string(),
                        x.get("id").or(x.get("key")).or(x.get("pane"))?.as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    let replace = p.get("replace").and_then(Value::as_bool).unwrap_or(false);
    let r = contribute(server, &info.plugin, &info.caps, &info.consent_id, &list, replace, &remove);
    if let Err(e) = &r
        && e.message.starts_with("capability_violation")
    {
        super::violation(server, &info, "ui.contribute", &e.message);
    }
    r
}

/// Announce a change (debounced to [`DEBOUNCE`] per plugin; a burst ends with one event).
fn changed(server: &Arc<Server>, plugin: &str) {
    let now = Instant::now();
    let delay = {
        let st = state(server);
        let mut c = st.ui.lock().unwrap();
        let pu = c.by_plugin.entry(plugin.to_string()).or_default();
        match pu.last_emit {
            Some(t) if now.duration_since(t) < DEBOUNCE => {
                if pu.scheduled {
                    return;
                }
                pu.scheduled = true;
                Some(DEBOUNCE - now.duration_since(t))
            }
            _ => {
                pu.last_emit = Some(now);
                None
            }
        }
    };
    match delay {
        None => emit_changed(server, plugin),
        Some(d) => {
            let (s, plugin) = (server.clone(), plugin.to_string());
            tokio::spawn(async move {
                tokio::time::sleep(d).await;
                {
                    let st = state(&s);
                    let mut c = st.ui.lock().unwrap();
                    if let Some(pu) = c.by_plugin.get_mut(&plugin) {
                        pu.scheduled = false;
                        pu.last_emit = Some(Instant::now());
                    }
                }
                emit_changed(&s, &plugin);
            });
        }
    }
}

fn emit_changed(server: &Server, plugin: &str) {
    let n = state(server)
        .ui
        .lock()
        .unwrap()
        .by_plugin
        .get(plugin)
        .map_or(0, |p| p.items.len());
    emit(
        server,
        "ui.contributions_changed",
        json!({"plugin": plugin}),
        super::actor(plugin, None),
        json!({"contributions": n}),
    );
}

/// Drop every contribution of `plugin` (process stopped, disabled, consent changed).
pub fn clear(server: &Server, plugin: &str) {
    let removed = {
        let st = state(server);
        let mut c = st.ui.lock().unwrap();
        c.by_plugin.remove(plugin)
    };
    if let Some(pu) = removed {
        if let Some(root) = pu.harness_root {
            crate::agents::manifests::set_plugin_root(&root, false);
        }
        if !pu.items.is_empty() {
            emit_changed(server, plugin);
        }
    }
}

/// Copy the plugin's harness manifests into a Vibeke-owned root (`<data>/harness-root/
/// .vibeke/harnesses/`) and load it like a trusted repository's (04): the plugin's consent is
/// the trust decision; the files are copies, so the plugin cannot swap them later.
fn sync_harness(server: &Server, plugin: &str, root: &Path) {
    let (data, _) = super::plugin_paths(server, plugin);
    let hroot = data.join("harness-root");
    let dir = hroot.join(".vibeke").join("harnesses");
    let _ = std::fs::remove_dir_all(&dir);
    let items: Vec<Value> = {
        let st = state(server);
        let c = st.ui.lock().unwrap();
        c.by_plugin
            .get(plugin)
            .map(|p| {
                p.items
                    .iter()
                    .filter(|((k, _), _)| k == "harness")
                    .map(|(_, v)| v.clone())
                    .collect()
            })
            .unwrap_or_default()
    };
    if items.is_empty() {
        crate::agents::manifests::set_plugin_root(&hroot, false);
        return;
    }
    let _ = std::fs::create_dir_all(&dir);
    for v in &items {
        let (Some(id), Some(rel)) = (v["id"].as_str(), v["path"].as_str()) else {
            continue;
        };
        let _ = std::fs::copy(root.join(rel), dir.join(format!("{id}.toml")));
    }
    crate::agents::manifests::set_plugin_root(&hroot, true);
    if let Some(pu) = state(server).ui.lock().unwrap().by_plugin.get_mut(plugin) {
        pu.harness_root = Some(hroot);
    }
}

/// Contributions of active plugins (under the consent they were made with).
pub fn list(server: &Server) -> Value {
    let reg = super::registry(server);
    let st = state(server);
    let c = st.ui.lock().unwrap();
    let mut out = vec![];
    for (plugin, pu) in &c.by_plugin {
        let live = reg.native.get(plugin).is_some_and(|e| {
            e.consent.as_ref().map(|x| x.consent_id.as_str()) == Some(pu.consent_id.as_str())
                && e.enabled
        });
        if !live {
            continue;
        }
        out.extend(pu.items.values().cloned());
    }
    json!({"contributions": out})
}

fn items_of(server: &Server, kind: &str) -> Vec<Value> {
    list(server)["contributions"]
        .as_array()
        .map(|a| a.iter().filter(|v| v["kind"] == kind).cloned().collect())
        .unwrap_or_default()
}

/// Palette entries from `palette_command` and `pane` contributions (`plugin.action.list`).
pub fn palette_actions(server: &Server, plugin: Option<&str>) -> Vec<Value> {
    let mut out = vec![];
    for v in items_of(server, "palette_command") {
        let pl = v["plugin_id"].as_str().unwrap_or_default();
        if plugin.is_some_and(|p| p != pl) {
            continue;
        }
        out.push(json!({
            "plugin_id": pl,
            "action_id": v["id"],
            "qualified_id": format!("{pl}.{}", v["id"].as_str().unwrap_or_default()),
            "title": v["title"],
            "contexts": v["contexts"],
            "available": true,
            "status": "active",
            "kind": "native",
            "contributed": true,
        }));
    }
    for v in items_of(server, "pane") {
        let pl = v["plugin_id"].as_str().unwrap_or_default();
        if plugin.is_some_and(|p| p != pl) {
            continue;
        }
        let id = format!("pane:{}", v["id"].as_str().unwrap_or_default());
        out.push(json!({
            "plugin_id": pl,
            "action_id": id,
            "qualified_id": format!("{pl}.{id}"),
            "title": format!("Open {}", v["title"].as_str().unwrap_or_default()),
            "contexts": ["global"],
            "available": true,
            "status": "active",
            "kind": "native",
            "contributed": true,
        }));
    }
    out
}

/// The manifest action a contributed palette command runs.
pub fn palette_target(server: &Server, plugin: &str, id: &str) -> Option<String> {
    items_of(server, "palette_command")
        .into_iter()
        .find(|v| v["plugin_id"] == plugin && v["id"] == id)
        .and_then(|v| v["action"].as_str().map(str::to_string))
}

/// `(plugin, key, "<plugin>.<action>")` of contributed key bindings.
pub fn keybinding_contribs(server: &Server) -> Vec<(String, String, String)> {
    items_of(server, "keybinding")
        .into_iter()
        .filter_map(|v| {
            let pl = v["plugin_id"].as_str()?.to_string();
            let key = v["key"].as_str()?.to_string();
            let a = v["action"].as_str()?;
            Some((pl.clone(), key, format!("{pl}.{a}")))
        })
        .collect()
}

/// Contributed link handlers in the `plugin.link_handler.list` shape.
pub fn link_handlers(server: &Server) -> Vec<Value> {
    items_of(server, "link_handler")
        .into_iter()
        .map(|v| {
            json!({
                "plugin_id": v["plugin_id"],
                "handler_id": v["id"],
                "title": v["title"],
                "pattern": v["pattern"],
                "action_id": v["action"],
                "available": true,
                "status": "active",
                "kind": "native",
            })
        })
        .collect()
}

/// `plugin.link.open` for a native handler: the URL must match; the action gets
/// `clicked_url`/`link_handler_id` in its context (`VIBEKE_PLUGIN_CONTEXT_JSON`).
pub async fn open_link(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    if ctx.pane_scope.is_some() {
        return Err(err(ErrorKind::PermissionDenied, "not available from a pane"));
    }
    let plugin = super::plugin_param(p)?;
    let handler = p
        .get("handler")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("handler is required"))?;
    let url = p
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("url is required"))?;
    let h = items_of(server, "link_handler")
        .into_iter()
        .find(|v| v["plugin_id"] == plugin && v["id"] == handler)
        .ok_or_else(|| err(ErrorKind::NotFound, format!("{plugin} has no link handler {handler}")))?;
    let re = regex::Regex::new(h["pattern"].as_str().unwrap_or("^$"))
        .map_err(|e| invalid(e.to_string()))?;
    if !re.is_match(url) {
        return Err(invalid(format!("{url} does not match handler {handler}")));
    }
    let mut ictx = super::actions::InvokeCtx::from_params(server, p);
    ictx.source = "link".into();
    ictx.extra.insert("clicked_url".into(), json!(url));
    ictx.extra.insert("link_handler_id".into(), json!(handler));
    let action = h["action"].as_str().unwrap_or_default().to_string();
    let (rec, _) = super::actions::invoke(server, plugin, &action, ictx).await?;
    Ok(json!({"log": rec}))
}

/// `ui.pane.open {plugin, pane, target?, direction?}`: open a contributed terminal pane next to
/// `target` (default: the focused pane). The command runs in the plugin dir as an ordinary
/// pane (pane scope, not the plugin's capabilities).
pub fn open_pane(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let plugin = super::plugin_param(p)?;
    let pane_id = p
        .get("pane")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("pane is required"))?;
    let pane_id = pane_id.strip_prefix("pane:").unwrap_or(pane_id);
    let (e, _) = super::active(server, plugin)?;
    let v = items_of(server, "pane")
        .into_iter()
        .find(|v| v["plugin_id"] == plugin && v["id"] == pane_id)
        .ok_or_else(|| err(ErrorKind::NotFound, format!("{plugin} contributes no pane {pane_id}")))?;
    let cmd: Vec<String> = v["command"]
        .as_array()
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let cmd = super::launch::resolve_argv(&e.root, &cmd);
    let target = crate::api::resolve_pane(
        server,
        ctx,
        Some(p.get("target").and_then(Value::as_str).unwrap_or("@focused")),
    )?;
    let dir = vk_proto::layout::Direction::parse(
        p.get("direction").and_then(Value::as_str).unwrap_or("right"),
    )
    .ok_or_else(|| invalid("direction must be right|down|left|up"))?;
    let pane = server
        .split_pane(
            &target.id,
            dir,
            0.5,
            Some(&e.root.to_string_lossy()),
            Some(cmd),
            v["title"].as_str().map(Into::into),
            None,
            &format!("plugin:{plugin}"),
        )
        .map_err(crate::api::internal)?;
    emit(
        server,
        "plugin.pane_opened",
        json!({"plugin": plugin, "pane": pane.id}),
        super::actor(plugin, None),
        json!({"entrypoint": pane_id}),
    );
    Ok(json!({"pane": pane}))
}

/// The contributed pane a palette id (`pane:<id>`) opens.
pub fn pane_for(action: &str) -> Option<&str> {
    action.strip_prefix("pane:")
}
