//! Native plugin capabilities (07 §7.1 `[capabilities]`, 09 §6): what a plugin asks for, how
//! risky each item is (shown at install, high risk in red), which approved set a newer manifest
//! widens (re-consent) and which API methods each capability opens.
//!
//! The method map is **default deny**: a method that is not listed is refused for every plugin,
//! and administrative methods (server control, registry changes, policy, trust, configuration,
//! integrations, audit, assistance) are refused whatever the plugin was granted.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// UI contribution kinds a plugin may request in `ui = [...]` (07 §7.4).
pub const UI_KINDS: &[&str] = &[
    "sidebar_section",
    "status_segment",
    "pane",
    "palette",
    "keybindings",
    "pane_decoration",
    "link_handler",
    "harness",
];

/// The `ui` capability a contribution kind needs (`palette_command` → `palette`, …).
pub fn ui_cap_for(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "sidebar_section" => "sidebar_section",
        "status_segment" => "status_segment",
        "pane" => "pane",
        "palette_command" => "palette",
        "keybinding" => "keybindings",
        "pane_decoration" => "pane_decoration",
        "link_handler" => "link_handler",
        "harness" => "harness",
        _ => return None,
    })
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Capabilities {
    /// Event type patterns (`agent.*`, `pane.created`, `*`).
    pub events_read: Vec<String>,
    /// `pane.read`, `search.query`, interaction/git/fs reads.
    pub panes_read: bool,
    /// `pane.send_text/keys/run`, splits, closes, renames, new tabs/workspaces.
    pub panes_write: bool,
    /// `agent.start/prompt/interrupt/…`.
    pub agents_control: bool,
    /// `interaction.answer/cancel` (high risk).
    pub interactions_answer: bool,
    /// `task.*` and `worktree.*` mutations.
    pub tasks_write: bool,
    /// `preview.*`, `browser.*` and screenshots (scripts need `browser_script`).
    pub preview_access: bool,
    /// `browser.eval` / page scripts (high risk).
    pub browser_script: bool,
    /// Hosts the plugin may reach (`*` = any). Enforced by the egress proxy when sandboxed.
    pub network: Vec<String>,
    /// Paths (`$PLUGIN_DATA`, `$PLUGIN_CONFIG`, `$HOME/...`, absolute; `:ro` suffix = read
    /// only). Enforced when sandboxed.
    pub filesystem: Vec<String>,
    /// UI contribution kinds ([`UI_KINDS`]).
    pub ui: Vec<String>,
    /// `plugin.kv.*`.
    pub storage: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    Low,
    Medium,
    High,
}

impl Risk {
    pub fn as_str(self) -> &'static str {
        match self {
            Risk::Low => "low",
            Risk::Medium => "medium",
            Risk::High => "high",
        }
    }
}

/// One requested item as shown at install / consent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Item {
    /// Stable name (`panes_write`, `network:api.github.com`, `events_read:agent.*`, …): also
    /// what `accept_capabilities` lists.
    pub name: String,
    pub description: String,
    pub risk: Risk,
}

/// `pattern` matches event type `ty`: `*`, a `prefix.*` glob or the exact name.
pub fn event_matches(pattern: &str, ty: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    match pattern.strip_suffix('*') {
        Some(prefix) => ty.starts_with(prefix),
        None => pattern == ty,
    }
}

/// Is every event `inner` can match also matched by `outer`?
fn pattern_covered(outer: &str, inner: &str) -> bool {
    if outer == "*" {
        return true;
    }
    match (outer.strip_suffix('*'), inner.strip_suffix('*')) {
        (Some(o), Some(i)) => i.starts_with(o),
        (Some(o), None) => inner.starts_with(o),
        (None, Some(_)) => false,
        (None, None) => outer == inner,
    }
}

/// `host` (lower-case) is matched by a network entry (`*`, `*.example.com`, `example.com`).
pub fn host_matches(entry: &str, host: &str) -> bool {
    let e = entry.trim().to_ascii_lowercase();
    let h = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if e == "*" {
        return true;
    }
    match e.strip_prefix("*.") {
        Some(base) => h.ends_with(&format!(".{base}")),
        None => e == h,
    }
}

fn host_covered(approved: &[String], entry: &str) -> bool {
    approved.iter().any(|a| {
        let a = a.trim().to_ascii_lowercase();
        let e = entry.trim().to_ascii_lowercase();
        a == "*"
            || a == e
            || match (a.strip_prefix("*."), e.strip_prefix("*.")) {
                (Some(ab), Some(eb)) => eb == ab || eb.ends_with(&format!(".{ab}")),
                (Some(_), None) => host_matches(&a, &e),
                _ => false,
            }
    })
}

impl Capabilities {
    /// Every requested item with its risk (09 §6: `interactions_answer`, `panes_write`,
    /// `agents_control`, `browser_script` and `network: *` are high).
    pub fn items(&self) -> Vec<Item> {
        let mut v = vec![];
        let mut push = |name: String, description: &str, risk: Risk| {
            v.push(Item {
                name,
                description: description.to_string(),
                risk,
            })
        };
        for e in &self.events_read {
            let risk = if e == "*" || e.starts_with("interaction") {
                Risk::Medium
            } else {
                Risk::Low
            };
            push(format!("events_read:{e}"), "read events of this type", risk);
        }
        if self.panes_read {
            push(
                "panes_read".into(),
                "read pane contents, search scrollback, read interactions, git and files",
                Risk::Medium,
            );
        }
        if self.panes_write {
            push(
                "panes_write".into(),
                "type into panes, run commands, split/close/rename panes",
                Risk::High,
            );
        }
        if self.agents_control {
            push(
                "agents_control".into(),
                "start, prompt and interrupt agents",
                Risk::High,
            );
        }
        if self.interactions_answer {
            push(
                "interactions_answer".into(),
                "answer agent approvals and questions on your behalf",
                Risk::High,
            );
        }
        if self.tasks_write {
            push(
                "tasks_write".into(),
                "create, change and finish tasks and worktrees",
                Risk::Medium,
            );
        }
        if self.preview_access {
            push(
                "preview_access".into(),
                "open previews and drive browser panes",
                Risk::Medium,
            );
        }
        if self.browser_script {
            push(
                "browser_script".into(),
                "run scripts in browser pages",
                Risk::High,
            );
        }
        for n in &self.network {
            let risk = if n.trim() == "*" {
                Risk::High
            } else {
                Risk::Medium
            };
            push(format!("network:{n}"), "connect to this host", risk);
        }
        for f in &self.filesystem {
            let own = f.starts_with("$PLUGIN_DATA") || f.starts_with("$PLUGIN_CONFIG");
            push(
                format!("filesystem:{f}"),
                "read/write this path",
                if own { Risk::Low } else { Risk::Medium },
            );
        }
        for u in &self.ui {
            push(format!("ui:{u}"), "show this kind of UI", Risk::Low);
        }
        if self.storage {
            push(
                "storage".into(),
                "keep data in the session's plugin KV store",
                Risk::Low,
            );
        }
        v
    }

    /// The highest risk of any requested item.
    pub fn max_risk(&self) -> Risk {
        self.items()
            .iter()
            .map(|i| i.risk)
            .max()
            .unwrap_or(Risk::Low)
    }

    /// Items `self` (a newer request) asks for that `approved` does not cover: a non-empty list
    /// requires re-consent (09 §6: widening needs consent, narrowing is silent).
    pub fn widened_from(&self, approved: &Capabilities) -> Vec<String> {
        let mut out = vec![];
        for e in &self.events_read {
            if !approved.events_read.iter().any(|a| pattern_covered(a, e)) {
                out.push(format!("events_read:{e}"));
            }
        }
        let flags = [
            ("panes_read", self.panes_read, approved.panes_read),
            ("panes_write", self.panes_write, approved.panes_write),
            ("agents_control", self.agents_control, approved.agents_control),
            (
                "interactions_answer",
                self.interactions_answer,
                approved.interactions_answer,
            ),
            ("tasks_write", self.tasks_write, approved.tasks_write),
            ("preview_access", self.preview_access, approved.preview_access),
            ("browser_script", self.browser_script, approved.browser_script),
            ("storage", self.storage, approved.storage),
        ];
        for (name, want, have) in flags {
            if want && !have {
                out.push(name.to_string());
            }
        }
        for n in &self.network {
            if !host_covered(&approved.network, n) {
                out.push(format!("network:{n}"));
            }
        }
        for f in &self.filesystem {
            let ro = f.ends_with(":ro");
            let base = f.trim_end_matches(":ro");
            let ok = approved.filesystem.iter().any(|a| {
                let a_ro = a.ends_with(":ro");
                let a_base = a.trim_end_matches(":ro");
                (base == a_base || base.starts_with(&format!("{}/", a_base.trim_end_matches('/'))))
                    && (ro || !a_ro)
            });
            if !ok {
                out.push(format!("filesystem:{f}"));
            }
        }
        for u in &self.ui {
            if !approved.ui.contains(u) {
                out.push(format!("ui:{u}"));
            }
        }
        out
    }

    /// Item names a consent must list ([`Item::name`]).
    pub fn names(&self) -> Vec<String> {
        self.items().into_iter().map(|i| i.name).collect()
    }

    /// Does `accepted` (item names, or `"*"` for everything shown) cover this request?
    /// Returns the names that were not accepted.
    pub fn not_accepted(&self, accepted: &[String]) -> Vec<String> {
        if accepted.iter().any(|a| a == "*") {
            return vec![];
        }
        self.names()
            .into_iter()
            .filter(|n| !accepted.contains(n))
            .collect()
    }

    pub fn reads_event(&self, ty: &str) -> bool {
        self.events_read.iter().any(|p| event_matches(p, ty))
    }

    pub fn allows_ui(&self, kind: &str) -> bool {
        ui_cap_for(kind).is_some_and(|c| self.ui.iter().any(|u| u == c))
    }

    pub fn allows_host(&self, host: &str) -> bool {
        self.network.iter().any(|e| host_matches(e, host))
    }

    /// Validation problems (unknown UI kinds, empty patterns).
    pub fn problems(&self) -> Vec<String> {
        let mut v = vec![];
        for u in &self.ui {
            if !UI_KINDS.contains(&u.as_str()) {
                v.push(format!("unknown ui kind `{u}` (one of {})", UI_KINDS.join(", ")));
            }
        }
        for e in &self.events_read {
            if e.trim().is_empty() || e.contains(char::is_whitespace) {
                v.push(format!("invalid events_read pattern `{e}`"));
            }
        }
        for n in &self.network {
            if n.trim().is_empty() || n.contains(['/', ' ']) {
                v.push(format!("invalid network host `{n}` (a host name, `*.domain` or `*`)"));
            }
        }
        for f in &self.filesystem {
            let base = f.trim_end_matches(":ro");
            if !(base.starts_with('/')
                || base.starts_with("$PLUGIN_DATA")
                || base.starts_with("$PLUGIN_CONFIG")
                || base.starts_with("$HOME"))
                || base.split('/').any(|c| c == "..")
            {
                v.push(format!(
                    "invalid filesystem path `{f}` (absolute, $PLUGIN_DATA, $PLUGIN_CONFIG or $HOME, no `..`)"
                ));
            }
        }
        v
    }
}

/// What calling a method needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    /// Read-only structure every plugin may see (ids, names, layout).
    Base,
    /// Needs this boolean capability.
    Cap(&'static str),
    /// `events.*`: `events_read` must cover every requested type.
    Events,
    /// `plugin.kv.*`.
    Storage,
    /// `ui.contribute`: checked per contribution kind.
    Ui,
    /// Never for plugins (administrative, or not mapped: default deny).
    Never,
}

/// The capability a method needs (default deny).
pub fn need(method: &str) -> Need {
    use Need::*;
    // Administrative surfaces first: never for plugins whatever they were granted.
    const ADMIN_PREFIXES: &[&str] = &[
        "server.",
        "session.",
        "config.",
        "policy.",
        "auth.",
        "audit.",
        "assistant.",
        "integration.",
        "compat.",
        "sandbox.",
        "gateway.",
        "debug.",
        "render.",
        "machine.",
        "group.",
        "client.focus",
    ];
    if matches!(method, "server.status" | "session.snapshot") {
        return Base;
    }
    if ADMIN_PREFIXES.iter().any(|p| method.starts_with(p)) {
        return Never;
    }
    match method {
        "client.hello" | "client.list" | "api.methods" | "api.schema" | "workspace.list"
        | "workspace.get" | "tab.list" | "pane.list" | "pane.get" | "pane.current"
        | "agent.list" | "agent.get" | "layout.export" | "task.list" | "task.get"
        | "notification.send" | "plugin.list" | "plugin.action.list" | "plugin.log.list"
        | "plugin.action" | "plugin.action.run" | "plugin.action.invoke" | "ui.contributions"
        | "worktree.list" | "status.segments" => Base,
        "plugin.kv.get" | "plugin.kv.set" | "plugin.kv.delete" | "plugin.kv.list" => Storage,
        "ui.contribute" => Ui,
        "events.read" | "events.wait" | "events.subscribe" | "events.unsubscribe" => Events,
        "pane.read" | "pane.wait_output" | "pane.wait_idle" | "search.query" | "agent.read"
        | "interaction.list" | "interaction.get" | "git.status" | "git.diff" | "git.log"
        | "fs.list" | "fs.read" | "notification.list" | "pane.can_see_paths" => {
            Cap("panes_read")
        }
        "pane.split" | "pane.close" | "pane.zoom" | "pane.resize" | "pane.equalize"
        | "pane.rename" | "pane.send_text" | "pane.send_keys" | "pane.send_bytes" | "pane.run"
        | "pane.mark_unread" | "pane.mark_seen" | "pane.pin" | "tab.create" | "workspace.create"
        | "notification.read" => Cap("panes_write"),
        "agent.start" | "agent.resume" | "agent.prompt" | "agent.interrupt" | "agent.send_keys"
        | "agent.rename" | "agent.release" | "agent.stop" | "agent.kill" => {
            Cap("agents_control")
        }
        "interaction.answer" | "interaction.cancel" => Cap("interactions_answer"),
        m if m.starts_with("task.") || m.starts_with("worktree.") => Cap("tasks_write"),
        "browser.eval" | "browser.script" => Cap("browser_script"),
        m if m.starts_with("preview.")
            || m.starts_with("browser.")
            || m.starts_with("screenshot.") =>
        {
            Cap("preview_access")
        }
        _ => Never,
    }
}

impl Capabilities {
    fn flag(&self, name: &str) -> bool {
        match name {
            "panes_read" => self.panes_read,
            "panes_write" => self.panes_write,
            "agents_control" => self.agents_control,
            "interactions_answer" => self.interactions_answer,
            "tasks_write" => self.tasks_write,
            "preview_access" => self.preview_access,
            "browser_script" => self.browser_script,
            "storage" => self.storage,
            _ => false,
        }
    }

    /// May a plugin with these (approved) capabilities call `method` with `params`? `Err` is
    /// the human-readable reason (the server turns it into `permission_denied` and a
    /// `plugin.capability_violation` event).
    pub fn check(&self, method: &str, params: &Value) -> Result<(), String> {
        match need(method) {
            Need::Base => Ok(()),
            Need::Never => Err(format!("{method} is not available to plugins")),
            Need::Cap(c) if self.flag(c) => Ok(()),
            Need::Cap(c) => Err(format!("{method} needs the `{c}` capability")),
            Need::Storage if self.storage => Ok(()),
            Need::Storage => Err(format!("{method} needs the `storage` capability")),
            Need::Ui if !self.ui.is_empty() => Ok(()),
            Need::Ui => Err(format!("{method} needs a `ui` capability")),
            Need::Events => {
                if method == "events.unsubscribe" {
                    return Ok(());
                }
                let types: Vec<String> = match params.get("types") {
                    Some(Value::Array(a)) => a
                        .iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect(),
                    Some(Value::String(s)) => s.split(',').map(str::to_string).collect(),
                    _ => vec![],
                };
                if types.is_empty() {
                    return Err(format!(
                        "{method} from a plugin must name `types` within its events_read ({})",
                        self.events_read.join(", ")
                    ));
                }
                match types
                    .iter()
                    .find(|t| !self.events_read.iter().any(|p| pattern_covered(p, t)))
                {
                    Some(t) => Err(format!("event type `{t}` is not in events_read")),
                    None => Ok(()),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn caps() -> Capabilities {
        Capabilities {
            events_read: vec!["agent.*".into(), "pane.created".into()],
            panes_read: true,
            network: vec!["api.github.com".into()],
            ui: vec!["status_segment".into()],
            storage: true,
            ..Default::default()
        }
    }

    #[test]
    fn risk_levels_mark_the_dangerous_ones_high() {
        let c = Capabilities {
            interactions_answer: true,
            panes_write: true,
            agents_control: true,
            browser_script: true,
            network: vec!["*".into(), "example.com".into()],
            ..Default::default()
        };
        let items = c.items();
        let risk = |n: &str| items.iter().find(|i| i.name == n).unwrap().risk;
        assert_eq!(risk("interactions_answer"), Risk::High);
        assert_eq!(risk("panes_write"), Risk::High);
        assert_eq!(risk("agents_control"), Risk::High);
        assert_eq!(risk("browser_script"), Risk::High);
        assert_eq!(risk("network:*"), Risk::High);
        assert_eq!(risk("network:example.com"), Risk::Medium);
        assert_eq!(c.max_risk(), Risk::High);
        assert_eq!(caps().max_risk(), Risk::Medium);
    }

    #[test]
    fn widening_needs_consent_narrowing_is_silent() {
        let approved = caps();
        assert!(approved.widened_from(&approved).is_empty());
        let mut narrower = approved.clone();
        narrower.panes_read = false;
        narrower.events_read = vec!["agent.started".into()];
        narrower.network.clear();
        assert!(narrower.widened_from(&approved).is_empty());
        let mut wider = approved.clone();
        wider.panes_write = true;
        wider.events_read.push("interaction.*".into());
        wider.network.push("example.com".into());
        wider.ui.push("sidebar_section".into());
        assert_eq!(
            wider.widened_from(&approved),
            vec![
                "events_read:interaction.*",
                "panes_write",
                "network:example.com",
                "ui:sidebar_section"
            ]
        );
        // `*` approved covers every narrower pattern, not the other way round.
        let star = Capabilities {
            events_read: vec!["*".into()],
            network: vec!["*".into()],
            ..Default::default()
        };
        assert!(caps().widened_from(&star).iter().all(|w| !w.starts_with("events_read")));
        assert!(star.widened_from(&caps()).contains(&"events_read:*".to_string()));
        // Read-only filesystem is covered by read-write, not the reverse.
        let rw = Capabilities {
            filesystem: vec!["$HOME/notes".into()],
            ..Default::default()
        };
        let ro = Capabilities {
            filesystem: vec!["$HOME/notes/sub:ro".into()],
            ..Default::default()
        };
        assert!(ro.widened_from(&rw).is_empty());
        assert_eq!(rw.widened_from(&ro), vec!["filesystem:$HOME/notes"]);
    }

    #[test]
    fn method_map_is_default_deny_and_admin_is_never() {
        let c = caps();
        assert!(c.check("pane.list", &json!({})).is_ok());
        assert!(c.check("pane.read", &json!({})).is_ok());
        assert!(c.check("pane.send_text", &json!({})).unwrap_err().contains("panes_write"));
        assert!(c.check("interaction.answer", &json!({})).is_err());
        assert!(c.check("plugin.kv.set", &json!({})).is_ok());
        assert!(c.check("some.new_method", &json!({})).is_err());
        let all = Capabilities {
            panes_write: true,
            agents_control: true,
            interactions_answer: true,
            tasks_write: true,
            preview_access: true,
            browser_script: true,
            ..Default::default()
        };
        for m in [
            "server.stop",
            "config.set",
            "policy.add",
            "auth.elevate",
            "plugin.install",
            "plugin.enable",
            "integration.install",
            "compat.herdr.call",
            "session.create",
            "assistant.run",
            "audit.tail",
        ] {
            assert!(all.check(m, &json!({})).is_err(), "{m}");
        }
        assert!(all.check("server.status", &json!({})).is_ok());
        assert!(all.check("interaction.answer", &json!({})).is_ok());
        assert!(all.check("task.create", &json!({})).is_ok());
        assert!(all.check("browser.eval", &json!({})).is_ok());
    }

    #[test]
    fn events_need_explicit_types_within_events_read() {
        let c = caps();
        assert!(c.check("events.subscribe", &json!({})).is_err());
        assert!(
            c.check("events.subscribe", &json!({"types": ["agent.started", "agent.*"]}))
                .is_ok()
        );
        assert!(c.check("events.read", &json!({"types": "pane.created"})).is_ok());
        assert!(
            c.check("events.wait", &json!({"types": ["interaction.opened"]}))
                .unwrap_err()
                .contains("interaction.opened")
        );
        assert!(c.check("events.subscribe", &json!({"types": ["*"]})).is_err());
        assert!(c.reads_event("agent.exited"));
        assert!(!c.reads_event("pane.closed"));
    }

    #[test]
    fn accept_lists_and_hosts() {
        let c = caps();
        assert!(c.not_accepted(&["*".into()]).is_empty());
        let missing = c.not_accepted(&["panes_read".into(), "storage".into()]);
        assert!(missing.contains(&"network:api.github.com".to_string()));
        assert!(host_matches("*.github.com", "api.github.com"));
        assert!(!host_matches("*.github.com", "github.com"));
        assert!(c.allows_host("API.GitHub.com"));
        assert!(c.allows_ui("status_segment"));
        assert!(!c.allows_ui("palette_command"));
        let bad = Capabilities {
            ui: vec!["banner".into()],
            filesystem: vec!["relative/x".into(), "/a/../b".into()],
            ..Default::default()
        };
        assert_eq!(bad.problems().len(), 3);
    }
}
