//! Plugin-provided agent views (`agent.view.set/clear`) and the default key bindings that
//! trusted, enabled plugins declare (`[[keys.command]]`), 07 §7.7.
//!
//! **Agent-list views (Herdr's form).** `agent.view.set {source, label?, filter?, sort[]}`
//! defines a named, filtered and sorted view of the agent list; there is at most one active
//! view per session, and a new one replaces it (Herdr's rule). Requests are validated like
//! Herdr validates them (`invalid_agent_view`): `source` is 1-120 ASCII letters, digits, `:`,
//! `.`, `_` or `-`; `label` at most 32 characters; filters at most 8 levels deep, 64 nodes and
//! 32 values per `in`; at most 8 sort fields; metadata tokens 1-32 letters, digits, `_` or
//! `-`. A `plugin:<id>` source needs that plugin registered and enabled. `agent.view.clear
//! {source?}` clears the active view when `source` is absent or matches. Both answer
//! `agent_view {active, source?, label?}`. The view is stored and announced
//! (`plugin.agent_view_changed`); Vibeke's sidebar does not apply its filter and sort yet.
//! A view set by a plugin goes away when that plugin is no longer active.
//!
//! **Run status lines (Vibeke's form).** A plugin invocation (identified by its broker, never
//! by request fields) can attach one short status line to an agent run: `agent.view.set
//! {target, text, detail?, tone?}`. The line (`text` <= 80 characters, controls and bidi
//! overrides stripped; `detail` <= 512 bytes for the peek; `tone` `info|ok|warn|error`) is
//! shown next to the run in the sidebar and in the peek. A plugin holds at most 16 views, one
//! per run. A view belongs to the grant it was set under: it disappears when the plugin is
//! disabled, untrusted, unlinked, uninstalled, re-reviewed (new grant) or its run is gone, and
//! credentials are redacted.

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

/// The active agent-list view (Herdr's `agent.view.set {source, …}`).
#[derive(Debug, Clone, PartialEq)]
pub struct ListView {
    /// The plugin that set it (`None` for a user or pane caller) and its grant.
    pub plugin: Option<(String, String)>,
    pub source: String,
    pub label: Option<String>,
    /// The normalized request: `{source, label?, filter?, sort}`.
    pub spec: Value,
    pub updated_ms: i64,
}

#[derive(Debug, Clone, Default)]
pub struct Views {
    /// Run status lines: `(plugin id, run id)` -> view.
    pub runs: HashMap<(String, String), View>,
    /// The active agent-list view; at most one per session.
    pub list: Option<ListView>,
}

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

/// Herdr's form when `source` is given and Vibeke's `text` is not.
fn herdr_form(p: &Value) -> bool {
    p.get("source").is_some() && p.get("text").is_none()
}

/// `agent.view.set`.
pub fn set(server: &Server, caller: &Caller, sn: &Snap, p: &Value) -> Result<Value, WireError> {
    if herdr_form(p) {
        return set_list(server, caller, p);
    }
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
        let v = &mut v.runs;
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

/// `agent.view.clear`. With `target` (Vibeke's form): that run's status line of the calling
/// plugin, answered `ok`. Otherwise Herdr's form: the active agent-list view when `source` is
/// absent or matches it, plus, without `source`, every status line of a calling plugin;
/// answered `agent_view` with what is active afterwards.
pub fn clear(server: &Server, caller: &Caller, sn: &Snap, p: &Value) -> Result<Value, WireError> {
    if sp(p, "target").or(sp(p, "agent_id")).is_some() {
        let (plugin, _) = plugin_of(caller)?;
        let target = run_target(sn, p)?.id;
        clear_runs(server, &plugin, Some(&target));
        return Ok(ok());
    }
    let source = match p.get("source") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(normalize_source(s).map_err(bad_view)?),
        Some(_) => return Err(bad_view("agent view source must be a string")),
    };
    if source.is_none()
        && let Some((plugin, _)) = &caller.plugin
    {
        clear_runs(server, plugin, None);
    }
    let cleared = {
        let st = state(server);
        let mut v = st.views.lock().unwrap();
        let hit = v
            .list
            .as_ref()
            .is_some_and(|l| source.as_deref().is_none_or(|s| s == l.source));
        if hit { v.list.take() } else { None }
    };
    if let Some(l) = cleared {
        emit(
            server,
            "plugin.agent_view_changed",
            json!({"source": l.source, "cleared": true}),
        );
    }
    Ok(list_result(server))
}

/// Remove a plugin's run status lines (one run, or all of them).
fn clear_runs(server: &Server, plugin: &str, run: Option<&str>) {
    let removed = {
        let st = state(server);
        let mut v = st.views.lock().unwrap();
        let before = v.runs.len();
        v.runs
            .retain(|(pl, r), _| !(pl == plugin && run.is_none_or(|t| t == r)));
        before - v.runs.len()
    };
    if removed > 0 {
        emit(
            server,
            "plugin.agent_view_changed",
            json!({"plugin": plugin, "run": run, "cleared": true}),
        );
    }
}

/// `agent_view {active, source?, label?}` for the active agent-list view.
fn list_result(server: &Server) -> Value {
    let st = state(server);
    let v = st.views.lock().unwrap();
    let mut r = json!({"active": v.list.is_some()});
    if let Some(l) = &v.list {
        r["source"] = json!(l.source);
        if let Some(label) = &l.label {
            r["label"] = json!(label);
        }
    }
    typed("agent_view", r)
}

// ---- agent-list views (Herdr's form) -------------------------------------------------------

const MAX_FILTER_DEPTH: usize = 8;
const MAX_FILTER_NODES: usize = 64;
const MAX_FILTER_VALUES: usize = 32;
const MAX_SORT_FIELDS: usize = 8;
const MAX_SOURCE_CHARS: usize = 120;
const MAX_LABEL_CHARS: usize = 32;
const BUILTIN_FIELDS: &[&str] = &[
    "status",
    "workspace_id",
    "tab_id",
    "pane_id",
    "agent",
    "seen",
    "state_change_seq",
];
const BUILTIN_SORT_FIELDS: &[&str] = &[
    "workspace_order",
    "tab_order",
    "pane_order",
    "attention",
    "status",
    "agent",
    "seen",
    "state_change_seq",
];

fn bad_view(msg: impl Into<String>) -> WireError {
    WireError::new("invalid_agent_view", msg)
}

fn normalize_source(source: &str) -> Result<String, String> {
    let s = source.trim();
    if s.is_empty()
        || s.chars().count() > MAX_SOURCE_CHARS
        || !s
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '.' | '_' | '-'))
    {
        return Err(format!(
            "agent view source must be non-empty, at most {MAX_SOURCE_CHARS} characters, and contain only ASCII letters, digits, colon, dot, underscore, or hyphen"
        ));
    }
    Ok(s.to_string())
}

fn normalize_label(label: &str) -> Result<String, String> {
    let l: String = label.trim().chars().filter(|c| !c.is_control()).collect();
    if l.is_empty() || l.chars().count() > MAX_LABEL_CHARS {
        return Err(format!(
            "agent view label must be non-empty and at most {MAX_LABEL_CHARS} characters"
        ));
    }
    Ok(l)
}

fn validate_token(token: &str) -> Result<(), String> {
    if token.is_empty()
        || token.len() > 32
        || !token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
    {
        return Err(format!("invalid agent view token `{token}`"));
    }
    Ok(())
}

/// A field (`AgentViewField` / `AgentViewSortField`): a built-in name or `{token}`. Returns
/// the built-in name, or `None` for a token.
fn field_name<'a>(f: &'a Value, builtins: &[&str]) -> Result<Option<&'a str>, String> {
    match f {
        Value::String(s) if builtins.contains(&s.as_str()) => Ok(Some(s.as_str())),
        Value::String(s) => Err(format!("unknown agent view field `{s}`")),
        Value::Object(o) => match o.get("token") {
            Some(Value::String(t)) => validate_token(t).map(|_| None),
            _ => Err("agent view field must be a built-in name or {token}".into()),
        },
        _ => Err("agent view field must be a built-in name or {token}".into()),
    }
}

/// Herdr's field/value pairing: `seen` takes a boolean, `state_change_seq` a number, the
/// current-workspace/tab contexts only their own field, every other field a string (`status`
/// one of Herdr's statuses).
fn validate_field_value(field: &Value, value: &Value) -> Result<(), String> {
    let name = field_name(field, BUILTIN_FIELDS)?;
    match (name, value) {
        (Some("workspace_id"), Value::Object(o))
            if o.get("context").and_then(Value::as_str) == Some("current_workspace_id") =>
        {
            Ok(())
        }
        (Some("tab_id"), Value::Object(o))
            if o.get("context").and_then(Value::as_str) == Some("current_tab_id") =>
        {
            Ok(())
        }
        (_, Value::Object(o)) if o.contains_key("context") => {
            Err("agent view context type does not match the selected field".into())
        }
        (Some("seen"), Value::Bool(_)) => Ok(()),
        (Some("state_change_seq"), Value::Number(n)) if n.is_u64() => Ok(()),
        (Some("status"), Value::String(v)) => {
            if matches!(
                v.as_str(),
                "idle" | "working" | "blocked" | "done" | "unknown"
            ) {
                Ok(())
            } else {
                Err(format!("unknown agent status `{v}`"))
            }
        }
        (Some("workspace_id" | "tab_id" | "pane_id" | "agent") | None, Value::String(_)) => Ok(()),
        _ => Err("agent view value type does not match the selected field".into()),
    }
}

fn validate_filter(f: &Value, depth: usize, nodes: &mut usize) -> Result<(), String> {
    if depth > MAX_FILTER_DEPTH {
        return Err(format!(
            "agent view filter may be nested at most {MAX_FILTER_DEPTH} levels"
        ));
    }
    *nodes += 1;
    if *nodes > MAX_FILTER_NODES {
        return Err(format!(
            "agent view filter may contain at most {MAX_FILTER_NODES} nodes"
        ));
    }
    let req = |k: &str| {
        f.get(k)
            .ok_or_else(|| format!("agent view filter needs `{k}`"))
    };
    match f.get("op").and_then(Value::as_str) {
        Some("all" | "any") => {
            let filters = req("filters")?
                .as_array()
                .ok_or("agent view filters must be a list")?;
            if filters.is_empty() {
                return Err("agent view all/any filters must not be empty".into());
            }
            for x in filters {
                validate_filter(x, depth + 1, nodes)?;
            }
            Ok(())
        }
        Some("not") => validate_filter(req("filter")?, depth + 1, nodes),
        Some("eq") => validate_field_value(req("field")?, req("value")?),
        Some("in") => {
            let values = req("values")?
                .as_array()
                .ok_or("agent view values must be a list")?;
            if values.is_empty() || values.len() > MAX_FILTER_VALUES {
                return Err(format!(
                    "agent view in filters require 1 to {MAX_FILTER_VALUES} values"
                ));
            }
            let field = req("field")?;
            for v in values {
                validate_field_value(field, v)?;
            }
            Ok(())
        }
        Some("exists") => field_name(req("field")?, BUILTIN_FIELDS).map(|_| ()),
        _ => Err("agent view filter op must be all, any, not, eq, in or exists".into()),
    }
}

/// Validate and normalize Herdr's `AgentViewSetParams`.
fn agent_view_spec(p: &Value) -> Result<(String, Option<String>, Value), String> {
    let source = normalize_source(
        p.get("source")
            .and_then(Value::as_str)
            .ok_or("agent view source must be a string")?,
    )?;
    let label = match p.get("label") {
        None | Some(Value::Null) => None,
        Some(Value::String(l)) => Some(normalize_label(l)?),
        Some(_) => return Err("agent view label must be a string".into()),
    };
    let filter = match p.get("filter") {
        None | Some(Value::Null) => None,
        Some(f) => {
            validate_filter(f, 1, &mut 0)?;
            Some(f.clone())
        }
    };
    let sort = match p.get("sort") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) => a.clone(),
        Some(_) => return Err("agent view sort must be a list".into()),
    };
    if sort.len() > MAX_SORT_FIELDS {
        return Err(format!(
            "agent view sort may contain at most {MAX_SORT_FIELDS} fields"
        ));
    }
    let mut sort_out = Vec::new();
    for s in &sort {
        field_name(
            s.get("field").ok_or("agent view sort needs `field`")?,
            BUILTIN_SORT_FIELDS,
        )?;
        let order = match s.get("order") {
            None | Some(Value::Null) => "asc",
            Some(Value::String(o)) if o == "asc" || o == "desc" => o.as_str(),
            Some(_) => return Err("agent view sort order must be asc or desc".into()),
        };
        sort_out.push(json!({"field": s["field"], "order": order}));
    }
    let mut spec = json!({"source": source, "sort": sort_out});
    if let Some(l) = &label {
        spec["label"] = json!(l);
    }
    if let Some(f) = filter {
        spec["filter"] = f;
    }
    Ok((source, label, spec))
}

/// `agent.view.set` in Herdr's form: replace the session's agent-list view.
fn set_list(server: &Server, caller: &Caller, p: &Value) -> Result<Value, WireError> {
    let (source, label, spec) = agent_view_spec(p).map_err(bad_view)?;
    if let Some(id) = source.strip_prefix("plugin:") {
        let reg = Registry::load(&plugin_dirs()).unwrap_or_default();
        let e = reg
            .plugins
            .get(id)
            .ok_or_else(|| WireError::new("plugin_not_found", "plugin not found"))?;
        if !e.enabled {
            return Err(WireError::new("plugin_disabled", "plugin is disabled"));
        }
    }
    let plugin = match (&caller.plugin, &caller.grant_id) {
        (Some((id, _)), Some(g)) if !g.is_empty() => Some((id.clone(), g.clone())),
        _ => None,
    };
    let view = ListView {
        plugin,
        source,
        label,
        spec,
        updated_ms: now_ms(),
    };
    let event = json!({
        "source": view.source,
        "label": view.label,
        "view": view.spec,
        "plugin": view.plugin.as_ref().map(|(id, _)| id),
        "updated_ms": view.updated_ms,
    });
    state(server).views.lock().unwrap().list = Some(view);
    emit(server, "plugin.agent_view_changed", event);
    Ok(list_result(server))
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
        let before = v.runs.len();
        v.runs.retain(|(pl, _), view| live(pl, &view.grant_id));
        // The agent-list view of a plugin goes with it (Herdr drops a `plugin:<id>` view when
        // the plugin is unlinked).
        let list_gone = v.list.as_ref().is_some_and(|l| {
            l.plugin.as_ref().is_some_and(|(pl, g)| !live(pl, g))
                || l.source
                    .strip_prefix("plugin:")
                    .is_some_and(|id| !reg.plugins.contains_key(id))
        });
        if list_gone {
            v.list = None;
        }
        before - v.runs.len() + usize::from(list_gone)
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
        .runs
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
    fn agent_list_views_are_validated_like_herdrs() {
        // herdr-projects' default view.
        let (source, label, spec) = agent_view_spec(&json!({
            "source": "herdr-projects",
            "label": "projects",
            "sort": [{"field": {"token": "hp_group"}, "order": "asc"}, {"field": {"token": "hp_rank"}}],
        }))
        .unwrap();
        assert_eq!(source, "herdr-projects");
        assert_eq!(label.as_deref(), Some("projects"));
        assert_eq!(spec["sort"][1]["order"], "asc", "order defaults to asc");
        assert!(spec.get("filter").is_none());
        // A filtered view.
        let (_, _, spec) = agent_view_spec(&json!({
            "source": " plugin:acme.views ",
            "filter": {"op": "all", "filters": [
                {"op": "eq", "field": {"token": "hp_project"}, "value": "web"},
                {"op": "in", "field": "status", "values": ["working", "blocked"]},
                {"op": "eq", "field": "workspace_id", "value": {"context": "current_workspace_id"}},
                {"op": "not", "filter": {"op": "exists", "field": "agent"}},
                {"op": "eq", "field": "seen", "value": false},
            ]},
        }))
        .unwrap();
        assert_eq!(spec["source"], "plugin:acme.views", "trimmed");
        assert_eq!(spec["filter"]["op"], "all");
        let bad = [
            json!({}),
            json!({"source": ""}),
            json!({"source": "has space"}),
            json!({"source": "x".repeat(121)}),
            json!({"source": "s", "label": ""}),
            json!({"source": "s", "label": "l".repeat(33)}),
            json!({"source": "s", "filter": {"op": "eq", "field": "status", "value": "busy"}}),
            json!({"source": "s", "filter": {"op": "eq", "field": "seen", "value": "yes"}}),
            json!({"source": "s", "filter": {"op": "eq", "field": "tab_id", "value": {"context": "current_workspace_id"}}}),
            json!({"source": "s", "filter": {"op": "all", "filters": []}}),
            json!({"source": "s", "filter": {"op": "in", "field": "agent", "values": []}}),
            json!({"source": "s", "filter": {"op": "exists", "field": {"token": "bad token"}}}),
            json!({"source": "s", "filter": {"op": "maybe"}}),
            json!({"source": "s", "sort": [{"field": "nope"}]}),
            json!({"source": "s", "sort": [{"field": "status", "order": "up"}]}),
            json!({"source": "s", "sort": vec![json!({"field": "status"}); 9]}),
        ];
        for b in bad {
            assert!(agent_view_spec(&b).is_err(), "{b}");
        }
        // Depth and node limits.
        let mut deep = json!({"op": "exists", "field": "agent"});
        for _ in 0..8 {
            deep = json!({"op": "not", "filter": deep});
        }
        assert!(agent_view_spec(&json!({"source": "s", "filter": deep})).is_err());
        let wide: Vec<Value> = (0..64)
            .map(|_| json!({"op": "exists", "field": "agent"}))
            .collect();
        assert!(
            agent_view_spec(&json!({"source": "s", "filter": {"op": "any", "filters": wide}}))
                .is_err(),
            "65 nodes"
        );
        assert!(herdr_form(&json!({"source": "s"})));
        assert!(!herdr_form(&json!({"target": "a1", "text": "hi"})));
    }

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
