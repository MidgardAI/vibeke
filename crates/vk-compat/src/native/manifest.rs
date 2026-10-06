//! The native plugin manifest, `vibeke-plugin.toml` (07 §7.1):
//!
//! ```toml
//! id = "demo.phone-bridge"        # reverse-DNS-ish, unique
//! name = "Phone bridge"
//! version = "0.3.0"
//! min_vibeke = "1.0.0"
//! platforms = ["linux", "macos"]
//! sandbox = true                    # opt into the OS sandbox early (09 §6)
//!
//! [[build]]
//! command = ["bun", "run", "build"]
//!
//! [[actions]]                       # Kind A: argv actions
//! id = "status"
//! title = "Show status"
//! contexts = ["workspace", "pane"]
//! command = ["bash", "scripts/status.sh"]   # omitted: the action is sent to the process
//! keybinding = "prefix+alt+s"
//!
//! [process]                         # Kind B: long-running process
//! command = ["bun", "run", "dist/main.js"]
//! restart = "on-failure"            # never | on-failure | always
//! autostart = true
//! watch = ["dist/**/*.js"]          # dev link: hot restart on change
//!
//! [[on]]                            # native event hooks (event JSON on stdin)
//! event = "worktree.created"
//! command = ["bash", "scripts/on-worktree.sh"]
//!
//! [limits]                          # resource limits for the process and argv commands
//! memory_mb = 1024
//! cpu_seconds = 3600
//! open_files = 1024
//!
//! [capabilities]                    # see [`super::caps`]
//! events_read = ["agent.*"]
//! storage = true
//! ```
//!
//! Unknown keys are kept as warnings (a newer manifest still loads); invalid values are errors.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::caps::Capabilities;
use crate::herdr::manifest::{CONTEXTS, PLATFORMS};
use crate::herdr::parse_version;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ManifestError {
    #[error("vibeke-plugin.toml: {0}")]
    Toml(String),
    #[error("vibeke-plugin.toml: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platforms: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Action {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default = "default_contexts")]
    pub contexts: Vec<String>,
    /// Argv; `None` routes the action to the running process (`plugin.action` request).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keybinding: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platforms: Option<Vec<String>>,
}

fn default_contexts() -> Vec<String> {
    vec!["global".into()]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Restart {
    Never,
    #[default]
    OnFailure,
    Always,
}

impl Restart {
    pub fn as_str(self) -> &'static str {
        match self {
            Restart::Never => "never",
            Restart::OnFailure => "on-failure",
            Restart::Always => "always",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Process {
    pub command: Vec<String>,
    #[serde(default)]
    pub restart: Restart,
    #[serde(default = "yes")]
    pub autostart: bool,
    /// Globs (relative to the plugin root) watched in dev-link mode; the manifest and the
    /// process command's files are always watched.
    #[serde(default)]
    pub watch: Vec<String>,
    /// Extra environment for the process.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hook {
    pub event: String,
    pub command: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub memory_mb: Option<u64>,
    pub cpu_seconds: Option<u64>,
    pub open_files: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_vibeke: Option<String>,
    #[serde(default)]
    pub platforms: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    /// Run the process and argv commands under the OS sandbox generated from the declared
    /// capabilities (09 §6 "opt-in early via `sandbox = true`").
    #[serde(default)]
    pub sandbox: bool,
    #[serde(default)]
    pub build: Vec<Step>,
    #[serde(default)]
    pub actions: Vec<Action>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<Process>,
    #[serde(default)]
    pub on: Vec<Hook>,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub capabilities: Capabilities,
    /// Unknown keys (not an error: a newer manifest still loads).
    #[serde(skip)]
    pub warnings: Vec<String>,
}

const TOP_KEYS: &[&str] = &[
    "id",
    "name",
    "version",
    "min_vibeke",
    "platforms",
    "description",
    "license",
    "homepage",
    "sandbox",
    "build",
    "actions",
    "process",
    "on",
    "limits",
    "capabilities",
];

/// `a.b-c`: lower-case letters, digits, `.`, `-`, `_`; at least one dot; no empty component.
pub fn valid_id(id: &str) -> bool {
    id.len() >= 3
        && id.len() <= 128
        && id.contains('.')
        && id.split('.').all(|c| !c.is_empty())
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-' | '_'))
        && id.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
}

fn valid_action_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_'))
}

fn on_platform(p: &Option<Vec<String>>, platform: &str) -> bool {
    p.as_ref().is_none_or(|ps| ps.iter().any(|x| x == platform))
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Manifest, ManifestError> {
        let table: toml::Table = text
            .parse()
            .map_err(|e: toml::de::Error| ManifestError::Toml(e.message().to_string()))?;
        let mut warnings = vec![];
        let mut known = toml::Table::new();
        for (k, v) in table {
            if TOP_KEYS.contains(&k.as_str()) {
                known.insert(k, v);
            } else {
                warnings.push(format!("unknown key `{k}` ignored"));
            }
        }
        let mut m: Manifest = toml::Value::Table(known)
            .try_into()
            .map_err(|e: toml::de::Error| ManifestError::Invalid(e.message().to_string()))?;
        m.warnings = warnings;
        m.validate()?;
        Ok(m)
    }

    fn validate(&self) -> Result<(), ManifestError> {
        let bad = |s: String| Err(ManifestError::Invalid(s));
        if !valid_id(&self.id) {
            return bad(format!(
                "id `{}` must be reverse-DNS-ish: lower-case letters, digits, `-`, `_`, with at least one `.`",
                self.id
            ));
        }
        if parse_version(&self.version).is_none() {
            return bad(format!("version `{}` is not x.y.z", self.version));
        }
        if let Some(v) = &self.min_vibeke
            && parse_version(v).is_none()
        {
            return bad(format!("min_vibeke `{v}` is not x.y.z"));
        }
        let platforms = |ps: &[String], what: &str| -> Result<(), ManifestError> {
            match ps.iter().find(|p| !PLATFORMS.contains(&p.as_str())) {
                Some(p) => Err(ManifestError::Invalid(format!(
                    "{what}: unknown platform `{p}` (one of {})",
                    PLATFORMS.join(", ")
                ))),
                None => Ok(()),
            }
        };
        platforms(&self.platforms, "platforms")?;
        let argv = |c: &[String], what: &str| -> Result<(), ManifestError> {
            if c.is_empty() || c[0].trim().is_empty() {
                Err(ManifestError::Invalid(format!("{what}: empty command")))
            } else {
                Ok(())
            }
        };
        for (i, b) in self.build.iter().enumerate() {
            argv(&b.command, &format!("build[{i}]"))?;
            platforms(b.platforms.as_deref().unwrap_or_default(), "build")?;
        }
        let mut seen = std::collections::BTreeSet::new();
        for a in &self.actions {
            if !valid_action_id(&a.id) {
                return bad(format!(
                    "action id `{}`: lower-case letters, digits, `-`, `_`",
                    a.id
                ));
            }
            if !seen.insert((a.id.clone(), a.platforms.clone())) {
                return bad(format!("duplicate action id `{}`", a.id));
            }
            if a.title.trim().is_empty() {
                return bad(format!("action `{}` has no title", a.id));
            }
            if let Some(c) = a.contexts.iter().find(|c| !CONTEXTS.contains(&c.as_str())) {
                return bad(format!(
                    "action `{}`: unknown context `{c}` (one of {})",
                    a.id,
                    CONTEXTS.join(", ")
                ));
            }
            match &a.command {
                Some(c) => argv(c, &format!("action `{}`", a.id))?,
                None if self.process.is_none() => {
                    return bad(format!(
                        "action `{}` has no command and the plugin has no [process] to send it to",
                        a.id
                    ));
                }
                None => {}
            }
            platforms(a.platforms.as_deref().unwrap_or_default(), "action")?;
        }
        if let Some(p) = &self.process {
            argv(&p.command, "process")?;
        }
        for h in &self.on {
            argv(&h.command, &format!("on `{}`", h.event))?;
            if h.event.trim().is_empty() {
                return bad("[[on]] without event".into());
            }
            if !self.capabilities.reads_event(&h.event) {
                return bad(format!(
                    "[[on]] event `{}` is not covered by capabilities.events_read",
                    h.event
                ));
            }
        }
        if let Some(p) = self.capabilities.problems().into_iter().next() {
            return bad(format!("capabilities: {p}"));
        }
        if self.limits.memory_mb == Some(0) || self.limits.open_files == Some(0) {
            return bad("limits: memory_mb and open_files must be > 0".into());
        }
        Ok(())
    }

    /// Can this manifest run here (platform, `min_vibeke` against `vibeke_version`)?
    pub fn compatible(&self, vibeke_version: &str, platform: &str) -> Result<(), String> {
        if !self.platforms.is_empty() && !self.platforms.iter().any(|p| p == platform) {
            return Err(format!(
                "{} supports {} only (this is {platform})",
                self.id,
                self.platforms.join(", ")
            ));
        }
        if let Some(min) = &self.min_vibeke
            && parse_version(min) > parse_version(vibeke_version)
        {
            return Err(format!(
                "{} requires Vibeke {min} or newer (this is {vibeke_version})",
                self.id
            ));
        }
        Ok(())
    }

    pub fn actions_on(&self, platform: &str) -> Vec<&Action> {
        self.actions
            .iter()
            .filter(|a| on_platform(&a.platforms, platform))
            .collect()
    }

    pub fn action(&self, id: &str, platform: &str) -> Option<&Action> {
        self.actions_on(platform).into_iter().find(|a| a.id == id)
    }

    pub fn build_on(&self, platform: &str) -> Vec<&Step> {
        self.build
            .iter()
            .filter(|b| on_platform(&b.platforms, platform))
            .collect()
    }

    /// `actions` or `process` (a plugin with a process is a process plugin).
    pub fn kind(&self) -> &'static str {
        if self.process.is_some() {
            "process"
        } else {
            "actions"
        }
    }

    /// Entrypoints shown at consent.
    pub fn entrypoints(&self, platform: &str) -> Vec<String> {
        let mut v: Vec<String> = self
            .build_on(platform)
            .iter()
            .map(|b| format!("build: {}", b.command.join(" ")))
            .collect();
        if let Some(p) = &self.process {
            v.push(format!(
                "process: {} (restart {}, autostart {})",
                p.command.join(" "),
                p.restart.as_str(),
                p.autostart
            ));
        }
        for a in self.actions_on(platform) {
            v.push(match &a.command {
                Some(c) => format!("action {}: {}", a.id, c.join(" ")),
                None => format!("action {}: sent to the process", a.id),
            });
        }
        for h in &self.on {
            v.push(format!("on {}: {}", h.event, h.command.join(" ")));
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"
id = "demo.phone-bridge"
name = "Phone bridge"
version = "0.3.0"
min_vibeke = "0.1.0"
platforms = ["linux", "macos"]
description = "bridge"
license = "MIT"
future_field = 1

[[build]]
command = ["bun", "run", "build"]

[[actions]]
id = "status"
title = "Show status"
contexts = ["workspace", "pane"]
command = ["bash", "scripts/status.sh"]
keybinding = "prefix+alt+s"

[[actions]]
id = "ping"
title = "Ping the process"

[process]
command = ["bun", "run", "dist/main.js"]
restart = "always"
watch = ["dist/*.js"]

[[on]]
event = "agent.started"
command = ["bash", "scripts/on.sh"]

[limits]
memory_mb = 512

[capabilities]
events_read = ["agent.*", "interaction.*", "pane.created"]
panes_read = true
network = ["api.github.com"]
filesystem = ["$PLUGIN_DATA"]
ui = ["sidebar_section", "status_segment", "palette", "keybindings"]
storage = true
"#;

    #[test]
    fn parses_the_spec_example() {
        let m = Manifest::parse(FULL).unwrap();
        assert_eq!(m.id, "demo.phone-bridge");
        assert_eq!(m.kind(), "process");
        assert_eq!(m.process.as_ref().unwrap().restart, Restart::Always);
        assert!(
            m.process.as_ref().unwrap().autostart,
            "autostart defaults on"
        );
        assert_eq!(m.actions.len(), 2);
        assert!(m.actions[1].command.is_none());
        assert_eq!(m.actions[1].contexts, vec!["global"]);
        assert_eq!(m.limits.memory_mb, Some(512));
        assert!(m.capabilities.storage);
        assert_eq!(m.warnings, vec!["unknown key `future_field` ignored"]);
        assert!(m.compatible("0.1.0", "macos").is_ok());
        assert!(
            m.compatible("0.0.9", "macos")
                .unwrap_err()
                .contains("requires Vibeke")
        );
        assert!(
            m.compatible("1.0.0", "windows")
                .unwrap_err()
                .contains("supports")
        );
        assert_eq!(m.entrypoints("linux").len(), 5);
    }

    #[test]
    fn rejects_invalid_manifests() {
        let base = "version = \"1.0.0\"\n";
        let cases = [
            ("id = \"nodot\"\n", "reverse-DNS"),
            ("id = \"Upper.Case\"\n", "reverse-DNS"),
            ("id = \"a.b\"\nversion = \"one\"\n", "not x.y.z"),
            (
                "id = \"a.b\"\n[[actions]]\nid = \"x\"\ntitle = \"X\"\n",
                "no [process]",
            ),
            (
                "id = \"a.b\"\n[[actions]]\nid = \"x\"\ntitle = \"X\"\ncommand = [\"a\"]\ncontexts = [\"desk\"]\n",
                "unknown context",
            ),
            (
                "id = \"a.b\"\n[[on]]\nevent = \"pane.created\"\ncommand = [\"x\"]\n",
                "events_read",
            ),
            (
                "id = \"a.b\"\n[capabilities]\nui = [\"banner\"]\n",
                "unknown ui kind",
            ),
            ("id = \"a.b\"\n[process]\ncommand = []\n", "empty command"),
            (
                "id = \"a.b\"\n[capabilities]\nroot = true\n",
                "unknown field",
            ),
        ];
        for (text, want) in cases {
            let full = if text.contains("version") {
                text.to_string()
            } else {
                format!("{base}{text}")
            };
            let e = Manifest::parse(&full).unwrap_err().to_string();
            assert!(e.contains(want), "{full}: {e}");
        }
        assert!(valid_id("acme.ci-status_2"));
        assert!(!valid_id(".a.b"));
        assert!(!valid_id("a..b"));
    }
}
