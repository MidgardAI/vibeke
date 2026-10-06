//! Herdr plugin manifests (`herdr-plugin.toml`, 07 §7.7 "Manifest and installation").
//!
//! Fields are the ones the 99 real manifests in `tests/compat/herdr/0.9.3/manifests/` use:
//! top-level `id name version min_herdr_version platforms description`, `[[build]]`,
//! `[[startup]]`, `[[actions]]`, `[[events]]`, `[[panes]]`, `[[link_handlers]]` and
//! `[[keys.command]]`. The manifest stays unchanged on disk; this is a read-only view.
//!
//! Validation follows the spec's list (identifiers, contexts, placements, regexes, platform
//! inheritance, `min_herdr_version` against the tested baseline). Rules the baseline schema
//! decides but the spec does not spell out — identifier character sets, default contexts and
//! placement — are marked *unverified* in the inventory and pinned by the differential suite.

use std::collections::BTreeSet;

use serde::Serialize;
use serde_json::Value;
use toml::Table;

use super::{BASELINE_VERSION, parse_version};

/// Contexts an action can declare (07 §7.7).
pub const CONTEXTS: &[&str] = &["global", "workspace", "tab", "pane", "selection"];
/// Plugin pane placements (07 §7.7).
pub const PLACEMENTS: &[&str] = &["overlay", "popup", "split", "tab", "zoomed"];
/// Platform names.
pub const PLATFORMS: &[&str] = &["linux", "macos", "windows"];
/// Default contexts when an action declares none (*unverified* against the baseline).
pub const DEFAULT_CONTEXTS: &[&str] = &["global"];
/// Default pane placement when none is declared (*unverified*).
pub const DEFAULT_PLACEMENT: &str = "overlay";

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ManifestError {
    #[error("herdr-plugin.toml: {0}")]
    Toml(String),
    #[error("herdr-plugin.toml: {0}")]
    Invalid(String),
    #[error(
        "plugin {id} requires Herdr {required}; the compatibility layer emulates Herdr {BASELINE_VERSION}"
    )]
    VersionTooNew { id: String, required: String },
}

/// One argv entrypoint (`[[build]]`, `[[startup]]`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Step {
    pub command: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platforms: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Action {
    pub id: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Declared contexts, or [`DEFAULT_CONTEXTS`] when the manifest omits them.
    pub contexts: Vec<String>,
    pub command: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platforms: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EventHook {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Baseline event name (`worktree.created`, `pane.agent_status_changed`, …).
    pub on: String,
    pub command: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platforms: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PaneDecl {
    pub id: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub placement: String,
    pub command: Vec<String>,
    /// Cells (`64`) or a percentage string (`"80%"`), as written.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platforms: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LinkHandler {
    pub id: String,
    pub title: String,
    pub pattern: String,
    /// Action id (bare, or qualified `<plugin>.<action>`).
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platforms: Option<Vec<String>>,
}

/// `[[keys.command]]` default binding.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct KeyCommand {
    pub key: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Manifest {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_herdr_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platforms: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub build: Vec<Step>,
    pub startup: Vec<Step>,
    pub actions: Vec<Action>,
    pub events: Vec<EventHook>,
    pub panes: Vec<PaneDecl>,
    pub link_handlers: Vec<LinkHandler>,
    pub keys: Vec<KeyCommand>,
    /// Non-fatal findings (unknown keys, unknown event names, unresolved key actions).
    pub warnings: Vec<String>,
}

/// Plugin and entrypoint identifiers: ASCII alphanumerics plus `. _ -` (entrypoints also allow
/// `:` — reviewr uses `review:staged`). *Unverified* against the baseline's exact rule.
fn valid_id(s: &str, allow_colon: bool) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') || (allow_colon && c == ':')
        })
}

struct Reader<'a> {
    path: String,
    t: &'a Table,
    seen: BTreeSet<&'a str>,
}

impl<'a> Reader<'a> {
    fn new(path: impl Into<String>, t: &'a Table) -> Self {
        Reader {
            path: path.into(),
            t,
            seen: BTreeSet::new(),
        }
    }
    fn key(&self, k: &str) -> String {
        if self.path.is_empty() {
            k.to_string()
        } else {
            format!("{}.{k}", self.path)
        }
    }
    fn get(&mut self, k: &'a str) -> Option<&'a toml::Value> {
        self.seen.insert(k);
        self.t.get(k)
    }
    fn string(&mut self, k: &'a str) -> Result<Option<String>, ManifestError> {
        match self.get(k) {
            None => Ok(None),
            Some(toml::Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(ManifestError::Invalid(format!(
                "`{}` must be a string",
                self.key(k)
            ))),
        }
    }
    fn req_string(&mut self, k: &'a str) -> Result<String, ManifestError> {
        self.string(k)?
            .ok_or_else(|| ManifestError::Invalid(format!("`{}` is required", self.key(k))))
    }
    fn strings(&mut self, k: &'a str) -> Result<Option<Vec<String>>, ManifestError> {
        match self.get(k) {
            None => Ok(None),
            Some(toml::Value::Array(a)) => a
                .iter()
                .map(|v| v.as_str().map(str::to_string))
                .collect::<Option<Vec<_>>>()
                .map(Some)
                .ok_or_else(|| {
                    ManifestError::Invalid(format!("`{}` must be an array of strings", self.key(k)))
                }),
            Some(_) => Err(ManifestError::Invalid(format!(
                "`{}` must be an array of strings",
                self.key(k)
            ))),
        }
    }
    fn command(&mut self) -> Result<Vec<String>, ManifestError> {
        let c = self.strings("command")?.ok_or_else(|| {
            ManifestError::Invalid(format!("`{}` is required", self.key("command")))
        })?;
        if c.is_empty() || c[0].is_empty() {
            return Err(ManifestError::Invalid(format!(
                "`{}` must name a program",
                self.key("command")
            )));
        }
        Ok(c)
    }
    fn platforms(&mut self) -> Result<Option<Vec<String>>, ManifestError> {
        let p = self.strings("platforms")?;
        if let Some(list) = &p {
            for x in list {
                if !PLATFORMS.contains(&x.as_str()) {
                    return Err(ManifestError::Invalid(format!(
                        "`{}`: unknown platform `{x}` (linux, macos, windows)",
                        self.key("platforms")
                    )));
                }
            }
        }
        Ok(p)
    }
    fn raw(&mut self, k: &'a str) -> Option<Value> {
        self.get(k)
            .and_then(|v| serde_json::to_value(v.clone()).ok())
    }
    fn unknown(&self, warnings: &mut Vec<String>) {
        for k in self.t.keys() {
            if !self.seen.contains(k.as_str()) {
                warnings.push(format!("unknown key `{}`", self.key(k)));
            }
        }
    }
}

fn tables<'a>(t: &'a Table, k: &str) -> Result<Vec<&'a Table>, ManifestError> {
    match t.get(k) {
        None => Ok(vec![]),
        Some(toml::Value::Array(a)) => a
            .iter()
            .map(|v| v.as_table())
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| ManifestError::Invalid(format!("`{k}` must be an array of tables"))),
        Some(_) => Err(ManifestError::Invalid(format!(
            "`{k}` must be an array of tables (`[[{k}]]`)"
        ))),
    }
}

fn unique<'a>(section: &str, ids: impl Iterator<Item = &'a str>) -> Result<(), ManifestError> {
    let mut seen = BTreeSet::new();
    for id in ids {
        if !seen.insert(id) {
            return Err(ManifestError::Invalid(format!(
                "duplicate {section} id `{id}`"
            )));
        }
    }
    Ok(())
}

/// What the focused UI can offer an action right now (07 §7.7 contexts): `global` actions are
/// always applicable; `workspace`, `tab` and `pane` need that object focused; `selection` needs
/// a text selection in the focused pane.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ActionContext {
    pub workspace: bool,
    pub tab: bool,
    pub pane: bool,
    pub selection: bool,
}

/// Whether an action declaring `contexts` applies in `c` (any declared context holds). An
/// action with no contexts at all applies everywhere, like `global`.
pub fn contexts_apply(contexts: &[String], c: ActionContext) -> bool {
    contexts.is_empty()
        || contexts.iter().any(|x| match x.as_str() {
            "workspace" => c.workspace,
            "tab" => c.tab,
            "pane" => c.pane,
            "selection" => c.selection,
            // `global` and anything a newer baseline adds: shown (never silently hidden).
            _ => true,
        })
}

impl Action {
    pub fn applies(&self, c: ActionContext) -> bool {
        contexts_apply(&self.contexts, c)
    }
}

impl Manifest {
    /// Declared default key bindings that bind a plugin action, resolved to the qualified
    /// action id (`<plugin>.<action>`): `(key, qualified, description)`. Bindings of other
    /// types, or naming an action the plugin does not declare, are not installable.
    pub fn plugin_key_bindings(&self) -> Vec<(String, String, Option<String>)> {
        self.keys
            .iter()
            .filter(|k| k.kind == "plugin_action")
            .filter_map(|k| {
                let a = self.resolve_action(&k.command)?;
                Some((
                    k.key.clone(),
                    format!("{}.{}", self.id, a.id),
                    k.description.clone(),
                ))
            })
            .collect()
    }

    /// Parse and validate manifest text.
    pub fn parse(text: &str) -> Result<Manifest, ManifestError> {
        let t: Table = toml::from_str(text).map_err(|e| ManifestError::Toml(e.to_string()))?;
        let mut warnings = Vec::new();
        let mut r = Reader::new("", &t);
        let id = r.req_string("id")?;
        if !valid_id(&id, false) {
            return Err(ManifestError::Invalid(format!(
                "invalid plugin id `{id}` (letters, digits, `.`, `_`, `-`)"
            )));
        }
        let name = r.string("name")?;
        let version = r.string("version")?;
        let min_herdr_version = r.string("min_herdr_version")?;
        let platforms = r.platforms()?;
        let description = r.string("description")?;
        if let Some(min) = &min_herdr_version {
            let need = parse_version(min).ok_or_else(|| {
                ManifestError::Invalid(format!("min_herdr_version `{min}` is not a version"))
            })?;
            if Some(need) > parse_version(BASELINE_VERSION) {
                return Err(ManifestError::VersionTooNew {
                    id,
                    required: min.clone(),
                });
            }
        }

        let steps =
            |section: &str, warnings: &mut Vec<String>| -> Result<Vec<Step>, ManifestError> {
                let mut out = Vec::new();
                for (i, s) in tables(&t, section)?.into_iter().enumerate() {
                    let mut r = Reader::new(format!("{section}[{i}]"), s);
                    out.push(Step {
                        command: r.command()?,
                        platforms: r.platforms()?,
                    });
                    r.unknown(warnings);
                }
                Ok(out)
            };
        let build = steps("build", &mut warnings)?;
        let startup = steps("startup", &mut warnings)?;
        r.seen.insert("build");
        r.seen.insert("startup");

        let mut actions = Vec::new();
        for (i, a) in tables(&t, "actions")?.into_iter().enumerate() {
            let mut r = Reader::new(format!("actions[{i}]"), a);
            let aid = r.req_string("id")?;
            if !valid_id(&aid, true) {
                return Err(ManifestError::Invalid(format!("invalid action id `{aid}`")));
            }
            let contexts = match r.strings("contexts")? {
                Some(c) => {
                    for x in &c {
                        if !CONTEXTS.contains(&x.as_str()) {
                            return Err(ManifestError::Invalid(format!(
                                "action `{aid}`: unknown context `{x}` ({})",
                                CONTEXTS.join(", ")
                            )));
                        }
                    }
                    c
                }
                None => DEFAULT_CONTEXTS.iter().map(|s| s.to_string()).collect(),
            };
            actions.push(Action {
                title: r.string("title")?.unwrap_or_else(|| aid.clone()),
                id: aid,
                description: r.string("description")?,
                contexts,
                command: r.command()?,
                platforms: r.platforms()?,
            });
            r.unknown(&mut warnings);
        }
        r.seen.insert("actions");

        let mut events = Vec::new();
        for (i, e) in tables(&t, "events")?.into_iter().enumerate() {
            let mut r = Reader::new(format!("events[{i}]"), e);
            let eid = r.string("id")?;
            if let Some(eid) = &eid
                && !valid_id(eid, true)
            {
                return Err(ManifestError::Invalid(format!(
                    "invalid event hook id `{eid}`"
                )));
            }
            let on = r.req_string("on")?;
            if !super::events::BASELINE_EVENTS.contains(&on.as_str()) {
                warnings.push(format!(
                    "events[{i}]: `{on}` is not a known baseline event; the hook will not fire"
                ));
            }
            events.push(EventHook {
                id: eid,
                on,
                command: r.command()?,
                platforms: r.platforms()?,
            });
            r.unknown(&mut warnings);
        }
        r.seen.insert("events");

        let mut panes = Vec::new();
        for (i, p) in tables(&t, "panes")?.into_iter().enumerate() {
            let mut r = Reader::new(format!("panes[{i}]"), p);
            let pid = r.req_string("id")?;
            if !valid_id(&pid, true) {
                return Err(ManifestError::Invalid(format!("invalid pane id `{pid}`")));
            }
            let placement = r
                .string("placement")?
                .unwrap_or_else(|| DEFAULT_PLACEMENT.to_string());
            if !PLACEMENTS.contains(&placement.as_str()) {
                return Err(ManifestError::Invalid(format!(
                    "pane `{pid}`: unknown placement `{placement}` ({})",
                    PLACEMENTS.join(", ")
                )));
            }
            panes.push(PaneDecl {
                title: r.string("title")?.unwrap_or_else(|| pid.clone()),
                id: pid,
                description: r.string("description")?,
                placement,
                command: r.command()?,
                width: r.raw("width"),
                height: r.raw("height"),
                platforms: r.platforms()?,
            });
            r.unknown(&mut warnings);
        }
        r.seen.insert("panes");

        let mut link_handlers = Vec::new();
        for (i, l) in tables(&t, "link_handlers")?.into_iter().enumerate() {
            let mut r = Reader::new(format!("link_handlers[{i}]"), l);
            let lid = r.req_string("id")?;
            if !valid_id(&lid, true) {
                return Err(ManifestError::Invalid(format!(
                    "invalid link handler id `{lid}`"
                )));
            }
            let pattern = r.req_string("pattern")?;
            regex::Regex::new(&pattern).map_err(|e| {
                ManifestError::Invalid(format!("link handler `{lid}`: invalid pattern: {e}"))
            })?;
            link_handlers.push(LinkHandler {
                title: r.string("title")?.unwrap_or_else(|| lid.clone()),
                id: lid,
                pattern,
                action: r.req_string("action")?,
                platforms: r.platforms()?,
            });
            r.unknown(&mut warnings);
        }
        r.seen.insert("link_handlers");

        let mut keys = Vec::new();
        if let Some(k) = r.get("keys") {
            let kt = k
                .as_table()
                .ok_or_else(|| ManifestError::Invalid("`keys` must be a table".into()))?;
            for (i, c) in tables(kt, "command")?.into_iter().enumerate() {
                let mut r = Reader::new(format!("keys.command[{i}]"), c);
                keys.push(KeyCommand {
                    key: r.req_string("key")?,
                    kind: r.req_string("type")?,
                    command: r.req_string("command")?,
                    description: r.string("description")?,
                });
                r.unknown(&mut warnings);
            }
            for k in kt.keys().filter(|k| *k != "command") {
                warnings.push(format!("unknown key `keys.{k}`"));
            }
        }
        r.unknown(&mut warnings);

        let m = Manifest {
            id,
            name,
            version,
            min_herdr_version,
            platforms,
            description,
            build,
            startup,
            actions,
            events,
            panes,
            link_handlers,
            keys,
            warnings,
        };
        // Ids are unique per platform: manifests declare per-platform twins with the same id.
        for pf in PLATFORMS {
            let on = |p: Option<&Vec<String>>| m.entry_on(p, pf);
            unique(
                "action",
                m.actions
                    .iter()
                    .filter(|a| on(a.platforms.as_ref()))
                    .map(|a| a.id.as_str()),
            )?;
            unique(
                "pane",
                m.panes
                    .iter()
                    .filter(|a| on(a.platforms.as_ref()))
                    .map(|a| a.id.as_str()),
            )?;
            unique(
                "link handler",
                m.link_handlers
                    .iter()
                    .filter(|a| on(a.platforms.as_ref()))
                    .map(|a| a.id.as_str()),
            )?;
            unique(
                "event hook",
                m.events
                    .iter()
                    .filter(|a| on(a.platforms.as_ref()))
                    .filter_map(|a| a.id.as_deref()),
            )?;
        }
        let mut warnings = m.warnings.clone();
        for l in &m.link_handlers {
            if m.resolve_action(&l.action).is_none() {
                return Err(ManifestError::Invalid(format!(
                    "link handler `{}`: unknown action `{}`",
                    l.id, l.action
                )));
            }
        }
        for k in &m.keys {
            if k.kind != "plugin_action" {
                warnings.push(format!(
                    "keys.command `{}`: type `{}` is not handled by plugins",
                    k.key, k.kind
                ));
            } else if m.resolve_action(&k.command).is_none() {
                warnings.push(format!(
                    "keys.command `{}`: action `{}` is not declared by this plugin",
                    k.key, k.command
                ));
            }
        }
        Ok(Manifest { warnings, ..m })
    }

    /// Read `<root>/herdr-plugin.toml` (or a direct manifest path).
    pub fn load(path: &std::path::Path) -> Result<(Manifest, String), ManifestError> {
        let file = if path.is_dir() {
            path.join(super::MANIFEST_FILE)
        } else {
            path.to_path_buf()
        };
        let text = std::fs::read_to_string(&file)
            .map_err(|e| ManifestError::Invalid(format!("{}: {e}", file.display())))?;
        Ok((Manifest::parse(&text)?, text))
    }

    /// Whether the plugin itself supports `platform` (absent = all platforms).
    pub fn supports(&self, platform: &str) -> bool {
        self.platforms
            .as_ref()
            .is_none_or(|p| p.iter().any(|x| x == platform))
    }

    /// Entry-level platform list overrides the plugin's; absent inherits it.
    pub fn entry_on(&self, entry: Option<&Vec<String>>, platform: &str) -> bool {
        match entry {
            Some(p) => p.iter().any(|x| x == platform),
            None => self.supports(platform),
        }
    }

    /// Resolve a bare (`open`) or qualified (`<plugin-id>.open`) action id on any platform.
    /// Qualification wins when the plugin id prefix matches; action ids may themselves contain
    /// dots, so a bare lookup is tried as well.
    pub fn resolve_action(&self, id: &str) -> Option<&Action> {
        let bare = id
            .strip_prefix(self.id.as_str())
            .and_then(|r| r.strip_prefix('.'));
        bare.and_then(|b| self.actions.iter().find(|a| a.id == b))
            .or_else(|| self.actions.iter().find(|a| a.id == id))
    }

    /// The action variant that runs on `platform` (manifests declare per-platform twins with
    /// the same id).
    pub fn action_for(&self, id: &str, platform: &str) -> Option<&Action> {
        let bare = id
            .strip_prefix(self.id.as_str())
            .and_then(|r| r.strip_prefix('.'))
            .unwrap_or(id);
        self.actions
            .iter()
            .filter(|a| a.id == bare || a.id == id)
            .find(|a| self.entry_on(a.platforms.as_ref(), platform))
    }

    /// Actions available on `platform`, one per id.
    pub fn actions_on(&self, platform: &str) -> Vec<&Action> {
        let mut seen = BTreeSet::new();
        self.actions
            .iter()
            .filter(|a| self.entry_on(a.platforms.as_ref(), platform))
            .filter(|a| seen.insert(a.id.as_str()))
            .collect()
    }

    pub fn build_on(&self, platform: &str) -> Vec<&Step> {
        self.build
            .iter()
            .filter(|s| self.entry_on(s.platforms.as_ref(), platform))
            .collect()
    }

    pub fn startup_on(&self, platform: &str) -> Vec<&Step> {
        self.startup
            .iter()
            .filter(|s| self.entry_on(s.platforms.as_ref(), platform))
            .collect()
    }

    /// Event hooks subscribed to `event` on `platform`.
    pub fn hooks_for(&self, event: &str, platform: &str) -> Vec<&EventHook> {
        self.events
            .iter()
            .filter(|e| e.on == event && self.entry_on(e.platforms.as_ref(), platform))
            .collect()
    }

    /// Every program this plugin can run, for the legacy trust review (09 §6).
    pub fn entrypoints(&self, platform: &str) -> Vec<String> {
        let mut out = Vec::new();
        for s in self.build_on(platform) {
            out.push(format!("build: {}", s.command.join(" ")));
        }
        for s in self.startup_on(platform) {
            out.push(format!("startup: {}", s.command.join(" ")));
        }
        for a in self.actions_on(platform) {
            out.push(format!("action {}: {}", a.id, a.command.join(" ")));
        }
        for e in self
            .events
            .iter()
            .filter(|e| self.entry_on(e.platforms.as_ref(), platform))
        {
            out.push(format!("event {}: {}", e.on, e.command.join(" ")));
        }
        for p in self
            .panes
            .iter()
            .filter(|p| self.entry_on(p.platforms.as_ref(), platform))
        {
            out.push(format!(
                "pane {} ({}): {}",
                p.id,
                p.placement,
                p.command.join(" ")
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINI: &str = r#"
id = "acme.demo"
name = "Demo"
version = "1.0.0"
min_herdr_version = "0.9.0"
platforms = ["linux", "macos"]

[[build]]
command = ["bash", "build.sh"]

[[build]]
platforms = ["windows"]
command = ["powershell", "build.ps1"]

[[actions]]
id = "open"
title = "Open"
contexts = ["pane", "workspace"]
command = ["bin/demo", "open"]

[[actions]]
id = "review:staged"
command = ["bin/demo", "staged"]

[[events]]
on = "worktree.created"
command = ["bin/demo", "auto"]

[[panes]]
id = "main"
title = "Demo"
placement = "popup"
width = "80%"
height = 20
command = ["bin/demo"]

[[link_handlers]]
id = "gh"
title = "GitHub"
pattern = '^https://github\.com/'
action = "acme.demo.open"

[[keys.command]]
key = "prefix+d"
type = "plugin_action"
command = "acme.demo.open"
"#;

    #[test]
    fn parses_all_sections() {
        let m = Manifest::parse(MINI).unwrap();
        assert_eq!(m.id, "acme.demo");
        assert_eq!(m.build.len(), 2);
        assert_eq!(m.build_on("linux").len(), 1);
        assert_eq!(m.build_on("windows").len(), 1, "entry platforms override");
        assert_eq!(m.actions[1].contexts, vec!["global"]);
        assert_eq!(m.actions[1].title, "review:staged");
        assert_eq!(m.panes[0].width, Some(Value::String("80%".into())));
        assert_eq!(m.panes[0].height, Some(serde_json::json!(20)));
        assert_eq!(m.resolve_action("acme.demo.open").unwrap().id, "open");
        assert_eq!(m.resolve_action("open").unwrap().id, "open");
        assert!(m.resolve_action("other.open").is_none());
        assert!(m.warnings.is_empty(), "{:?}", m.warnings);
        assert_eq!(m.hooks_for("worktree.created", "linux").len(), 1);
        assert!(m.hooks_for("worktree.created", "windows").is_empty());
        assert!(
            m.entrypoints("linux")
                .iter()
                .any(|e| e.starts_with("pane main (popup)"))
        );
    }

    #[test]
    fn contexts_decide_where_an_action_applies() {
        let m = Manifest::parse(MINI).unwrap();
        let open = m.resolve_action("open").unwrap(); // pane, workspace
        let staged = m.resolve_action("review:staged").unwrap(); // global
        let nothing = ActionContext::default();
        let ws = ActionContext {
            workspace: true,
            ..Default::default()
        };
        let sel = ActionContext {
            pane: true,
            selection: true,
            ..Default::default()
        };
        assert!(!open.applies(nothing));
        assert!(open.applies(ws));
        assert!(open.applies(sel), "pane context");
        assert!(staged.applies(nothing), "global applies everywhere");
        let only_sel = vec!["selection".to_string()];
        assert!(!contexts_apply(
            &only_sel,
            ActionContext {
                pane: true,
                ..Default::default()
            }
        ));
        assert!(contexts_apply(&only_sel, sel));
        let tab_only = vec!["tab".to_string()];
        assert!(!contexts_apply(&tab_only, ws));
        assert!(contexts_apply(
            &tab_only,
            ActionContext {
                tab: true,
                ..Default::default()
            }
        ));
    }

    #[test]
    fn default_key_bindings_resolve_to_qualified_actions() {
        let m = Manifest::parse(MINI).unwrap();
        assert_eq!(
            m.plugin_key_bindings(),
            vec![("prefix+d".to_string(), "acme.demo.open".to_string(), None)]
        );
        let m = Manifest::parse(
            "id = 'a'\n[[actions]]\nid = 'x'\ncommand = ['x']\n[[keys.command]]\nkey = 'prefix+x'\ntype = 'plugin_action'\ncommand = 'nope'\n[[keys.command]]\nkey = 'prefix+y'\ntype = 'shell'\ncommand = 'ls'\n[[keys.command]]\nkey = 'prefix+z'\ntype = 'plugin_action'\ncommand = 'x'\ndescription = 'Do x'\n",
        )
        .unwrap();
        assert_eq!(
            m.plugin_key_bindings(),
            vec![("prefix+z".into(), "a.x".into(), Some("Do x".into()))],
            "unresolved and non-plugin bindings are not installable"
        );
    }

    #[test]
    fn rejects_invalid() {
        let bad = |s: &str| Manifest::parse(s).unwrap_err();
        assert!(matches!(bad("name = 'x'"), ManifestError::Invalid(_)));
        assert!(matches!(bad("id = 'has space'"), ManifestError::Invalid(_)));
        assert!(matches!(
            bad("id = 'a'\nmin_herdr_version = '0.10.0'"),
            ManifestError::VersionTooNew { .. }
        ));
        assert!(matches!(
            bad("id = 'a'\n[[actions]]\nid = 'x'\ncontexts = ['nowhere']\ncommand = ['x']"),
            ManifestError::Invalid(_)
        ));
        assert!(matches!(
            bad("id = 'a'\n[[actions]]\nid = 'x'\ncommand = []"),
            ManifestError::Invalid(_)
        ));
        assert!(matches!(
            bad("id = 'a'\n[[panes]]\nid = 'p'\nplacement = 'sideways'\ncommand = ['x']"),
            ManifestError::Invalid(_)
        ));
        assert!(matches!(
            bad(
                "id = 'a'\n[[actions]]\nid = 'x'\ncommand = ['x']\n[[actions]]\nid = 'x'\ncommand = ['y']\nplatforms = ['linux']"
            ),
            ManifestError::Invalid(_)
        ));
        assert!(matches!(
            bad("id = 'a'\n[[link_handlers]]\nid = 'l'\npattern = '('\naction = 'x'"),
            ManifestError::Invalid(_)
        ));
        assert!(matches!(
            bad("id = 'a'\nplatforms = ['beos']"),
            ManifestError::Invalid(_)
        ));
        assert!(matches!(bad("id = "), ManifestError::Toml(_)));
    }

    #[test]
    fn warnings_for_unknown_keys_and_events() {
        let m = Manifest::parse(
            "id = 'a'\nfuture = 1\n[[events]]\non = 'galaxy.exploded'\ncommand = ['x']\nextra = true",
        )
        .unwrap();
        assert!(
            m.warnings.iter().any(|w| w.contains("`future`")),
            "{:?}",
            m.warnings
        );
        assert!(m.warnings.iter().any(|w| w.contains("events[0].extra")));
        assert!(m.warnings.iter().any(|w| w.contains("galaxy.exploded")));
    }
}
