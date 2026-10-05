//! Herdr `config.toml` importer (08 §12).
//!
//! Herdr's real key names come from its documented default template (`herdr --default-config`):
//! sidebar sizes are flat `ui.sidebar_*` keys, `ui.confirm_close` is a bool, toasts live in
//! `ui.toast.delivery`, and so on. Everything is mapped through the same validation Vibeke
//! applies on load, so the produced file always loads.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

use vk_config::Config;

/// What the importer did.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportReport {
    /// The resulting configuration (Vibeke defaults plus the imported settings).
    pub config: Config,
    /// TOML text containing only the non-default imported keys, ready to write.
    pub toml: String,
    /// `(herdr key, vibeke key)` pairs that were carried over.
    pub mapped: Vec<(String, String)>,
    /// Importable Vibeke keys the Herdr file did not set, so Vibeke's default applies.
    pub defaulted: Vec<String>,
    /// Herdr keys with no Vibeke equivalent, or values Vibeke rejected (with the reason).
    pub unsupported: Vec<String>,
    /// Load warnings for the produced config plus notes about differing defaults.
    pub warnings: Vec<String>,
    /// The input could not be read at all (invalid TOML); nothing was imported.
    pub errors: Vec<String>,
}

/// Vibeke keys covered by the §12 mapping table, used to report what stayed default.
const IMPORTABLE: &[&str] = &[
    "onboarding",
    "theme.name",
    "theme.auto_switch",
    "theme.dark_name",
    "theme.light_name",
    "terminal.default_shell",
    "terminal.shell_mode",
    "terminal.new_cwd",
    "update.channel",
    "update.version_check",
    "update.manifest_check",
    "keys.prefix",
    "keys.remote_image_paste",
    "tasks.root",
    "ui.sidebar.width",
    "ui.sidebar.min_width",
    "ui.sidebar.max_width",
    "ui.sidebar.collapsed",
];

/// Settings where Herdr's own default differs from Vibeke's: `(herdr key, vibeke key, note)`.
const DEFAULT_DIFFERENCES: &[(&str, &str, &str)] = &[
    (
        "theme.auto_switch",
        "theme.auto_switch",
        "Herdr defaults to false, Vibeke to true",
    ),
    (
        "ui.sidebar_width",
        "ui.sidebar.width",
        "Herdr defaults to 26, Vibeke to 28",
    ),
    (
        "ui.sidebar_max_width",
        "ui.sidebar.max_width",
        "Herdr defaults to 36, Vibeke to 48",
    ),
    (
        "ui.copy_on_select",
        "clipboard.copy_on_select",
        "Herdr defaults to true, Vibeke to false",
    ),
    (
        "worktrees.directory",
        "tasks.root",
        "Herdr defaults to ~/.herdr/worktrees, Vibeke to ~/.vibeke/worktrees",
    ),
];

struct Importer<'a> {
    raw: &'a toml::Table,
    out: toml::Table,
    consumed: BTreeSet<String>,
    mapped: Vec<(String, String)>,
    unsupported: Vec<String>,
}

/// Import a Herdr `config.toml`.
pub fn import_config(herdr_config_toml: &str) -> ImportReport {
    let raw: toml::Table = match toml::from_str(herdr_config_toml) {
        Ok(t) => t,
        Err(e) => {
            return ImportReport {
                config: Config::default(),
                toml: String::new(),
                mapped: vec![],
                defaulted: vec![],
                unsupported: vec![],
                warnings: vec![],
                errors: vec![format!("Herdr config is not valid TOML: {e}")],
            };
        }
    };
    let mut imp = Importer {
        raw: &raw,
        out: toml::Table::new(),
        consumed: BTreeSet::new(),
        mapped: Vec::new(),
        unsupported: Vec::new(),
    };
    imp.run();

    let mut warnings = Vec::new();
    for (herdr, _, note) in DEFAULT_DIFFERENCES {
        if get(&raw, herdr).is_none() {
            warnings.push(format!(
                "{herdr} is not set in your Herdr config; {note}. Vibeke's default applies."
            ));
        }
    }

    // Collect unsupported keys before consuming `imp`.
    let mut unsupported = std::mem::take(&mut imp.unsupported);
    collect_unsupported(&raw, "", &imp.consumed, &mut unsupported);

    // Drop values equal to the Vibeke defaults.
    let mut out = imp.out;
    if let toml::Value::Table(defaults) = Config::default().to_value() {
        strip_defaults(&mut out, &defaults);
    }
    let body = if out.is_empty() {
        String::new()
    } else {
        toml::to_string(&out).unwrap_or_default()
    };
    let text = format!("# Imported from Herdr by `vibeke import herdr`.\n{body}");

    let mut errors = Vec::new();
    let config = match Config::parse(&text, Path::new("config.imported.toml")) {
        Ok((c, w)) => {
            warnings.extend(w.into_iter().map(|w| w.to_string()));
            c
        }
        Err(e) => {
            errors.push(format!("internal: imported config failed validation: {e}"));
            Config::default()
        }
    };

    let mapped = imp.mapped;
    let set_targets: BTreeSet<&str> = mapped.iter().map(|(_, to)| to.as_str()).collect();
    let defaulted = IMPORTABLE
        .iter()
        .filter(|k| !set_targets.contains(*k))
        .map(|k| k.to_string())
        .collect();

    ImportReport {
        config,
        toml: text,
        mapped,
        defaulted,
        unsupported,
        warnings,
        errors,
    }
}

impl Importer<'_> {
    fn run(&mut self) {
        // Plain renames.
        for (from, to) in [
            ("onboarding", "onboarding"),
            ("theme.name", "theme.name"),
            ("theme.auto_switch", "theme.auto_switch"),
            ("theme.dark_name", "theme.dark_name"),
            ("theme.light_name", "theme.light_name"),
            ("terminal.default_shell", "terminal.default_shell"),
            ("terminal.shell_mode", "terminal.shell_mode"),
            ("terminal.new_cwd", "terminal.new_cwd"),
            ("update.channel", "update.channel"),
            ("update.version_check", "update.version_check"),
            ("update.manifest_check", "update.manifest_check"),
            ("worktrees.directory", "tasks.root"),
            ("ui.sidebar_width", "ui.sidebar.width"),
            ("ui.sidebar_min_width", "ui.sidebar.min_width"),
            ("ui.sidebar_max_width", "ui.sidebar.max_width"),
            ("ui.sidebar_start_collapsed", "ui.sidebar.collapsed"),
            ("ui.copy_on_select", "clipboard.copy_on_select"),
            ("ui.tab_bar_position", "ui.tabs.position"),
        ] {
            self.simple(from, to);
        }

        self.theme_custom();
        self.accent();
        self.confirm_close();
        self.toast();
        self.sound();
        self.resume();
        self.keys();
        self.sidebar_tokens();
    }

    fn consume(&mut self, path: &str) {
        self.consumed.insert(path.to_string());
    }

    fn simple(&mut self, from: &str, to: &str) {
        if let Some(v) = get(self.raw, from).cloned() {
            self.consume(from);
            self.set(from, to, v);
        }
    }

    /// Validate-then-set. A value Vibeke rejects goes to `unsupported` with the reason.
    fn set(&mut self, from: &str, to: &str, value: toml::Value) {
        let mut candidate = self.out.clone();
        set_path(&mut candidate, to, value);
        match validate(&candidate) {
            Ok(()) => {
                self.out = candidate;
                self.mapped.push((from.to_string(), to.to_string()));
            }
            Err(e) => self.unsupported.push(format!("{from}: {e}")),
        }
    }

    fn push(&mut self, from: &str, to: &str, value: toml::Value) {
        let mut candidate = self.out.clone();
        push_path(&mut candidate, to, value);
        match validate(&candidate) {
            Ok(()) => {
                self.out = candidate;
                self.mapped.push((from.to_string(), to.to_string()));
            }
            Err(e) => self.unsupported.push(format!("{from}: {e}")),
        }
    }

    fn theme_custom(&mut self) {
        let Some(toml::Value::Table(t)) = get(self.raw, "theme.custom").cloned() else {
            return;
        };
        self.consume("theme.custom");
        for (k, v) in t {
            self.set(
                &format!("theme.custom.{k}"),
                &format!("theme.custom.{k}"),
                v,
            );
        }
    }

    fn accent(&mut self) {
        if let Some(v) = get(self.raw, "ui.accent").cloned() {
            self.consume("ui.accent");
            if get(self.raw, "theme.custom.accent").is_some() {
                self.unsupported
                    .push("ui.accent: theme.custom.accent takes precedence".into());
            } else {
                self.set("ui.accent", "theme.custom.accent", v);
            }
        }
    }

    fn confirm_close(&mut self) {
        if let Some(v) = get(self.raw, "ui.confirm_close").cloned() {
            self.consume("ui.confirm_close");
            match v.as_bool() {
                Some(b) => self.set(
                    "ui.confirm_close",
                    "ui.confirm_close",
                    toml::Value::String(if b { "running" } else { "never" }.into()),
                ),
                None => self
                    .unsupported
                    .push("ui.confirm_close: expected a boolean".into()),
            }
        }
    }

    fn toast(&mut self) {
        if let Some(v) = get(self.raw, "ui.toast.delivery").cloned() {
            self.consume("ui.toast.delivery");
            let channel = match v.as_str() {
                // In-app toasts are always on in Vibeke; `channel` only governs outside delivery.
                Some("off") | Some("herdr") => "none",
                Some("terminal") => "osc",
                Some("system") => "native",
                _ => {
                    self.unsupported
                        .push("ui.toast.delivery: expected off | herdr | terminal | system".into());
                    return;
                }
            };
            self.set(
                "ui.toast.delivery",
                "notifications.channel",
                toml::Value::String(channel.into()),
            );
        }
    }

    fn sound(&mut self) {
        let enabled = get(self.raw, "ui.sound.enabled").and_then(|v| v.as_bool());
        let path = get(self.raw, "ui.sound.path")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        if enabled.is_some() {
            self.consume("ui.sound.enabled");
        }
        if path.is_some() {
            self.consume("ui.sound.path");
        }
        if enabled == Some(false) {
            self.set(
                "ui.sound.enabled",
                "notifications.sound",
                toml::Value::String("none".into()),
            );
        } else if let Some(p) = path {
            self.set(
                "ui.sound.path",
                "notifications.sound",
                toml::Value::String(p),
            );
        }
    }

    fn resume(&mut self) {
        if let Some(v) = get(self.raw, "session.resume_agents_on_restore").cloned() {
            self.consume("session.resume_agents_on_restore");
            match v.as_bool() {
                Some(b) => self.set(
                    "session.resume_agents_on_restore",
                    "agents.resume_on_restart",
                    toml::Value::String(if b { "always" } else { "never" }.into()),
                ),
                None => self
                    .unsupported
                    .push("session.resume_agents_on_restore: expected a boolean".into()),
            }
        }
    }

    fn keys(&mut self) {
        let Some(toml::Value::Table(keys)) = get(self.raw, "keys").cloned() else {
            return;
        };
        self.simple("keys.prefix", "keys.prefix");

        // Canonical names first so an explicit canonical entry beats a legacy alias.
        let mut ordered: Vec<(&String, &toml::Value)> = keys.iter().collect();
        ordered.sort_by_key(|(k, _)| vk_config::canonical_action(k) != k.as_str());
        for (k, v) in ordered {
            if matches!(k.as_str(), "prefix" | "indexed" | "command") {
                continue;
            }
            let from = format!("keys.{k}");
            self.consume(&from);
            let canon = vk_config::canonical_action(k).to_string();
            let Some(binding) = v.as_str() else {
                self.unsupported
                    .push(format!("{from}: expected a binding string"));
                continue;
            };
            if canon != *k && keys.contains_key(&canon) {
                self.unsupported.push(format!(
                    "{from}: `{canon}` is also set and takes precedence"
                ));
            } else if vk_config::is_known_action(&canon) {
                self.set(
                    &from,
                    &format!("keys.{canon}"),
                    toml::Value::String(binding.into()),
                );
            } else if k.starts_with("navigate_") {
                self.set(
                    &from,
                    &format!("keys.navigate.{k}"),
                    toml::Value::String(binding.into()),
                );
            } else {
                self.unsupported
                    .push(format!("{from}: no such action in Vibeke"));
            }
        }

        // Legacy [keys.indexed]: tabs/workspaces/agents -> ranges.
        if let Some(toml::Value::Table(ix)) = keys.get("indexed") {
            self.consume("keys.indexed");
            for (field, action) in [
                ("tabs", "switch_tab"),
                ("workspaces", "switch_workspace"),
                ("agents", "focus_agent"),
            ] {
                let from = format!("keys.indexed.{field}");
                let Some(v) = ix.get(field) else { continue };
                let Some(mods) = v.as_str() else {
                    self.unsupported.push(format!("{from}: expected a string"));
                    continue;
                };
                if mods.trim().is_empty() {
                    continue;
                }
                if keys.contains_key(action) {
                    self.unsupported.push(format!(
                        "{from}: keys.{action} is set explicitly and takes precedence"
                    ));
                    continue;
                }
                self.set(
                    &from,
                    &format!("keys.{action}"),
                    toml::Value::String(format!("{}+1..9", mods.trim())),
                );
            }
            for k in ix.keys() {
                if !matches!(k.as_str(), "tabs" | "workspaces" | "agents") {
                    self.unsupported.push(format!("keys.indexed.{k}"));
                }
            }
        }

        // [[keys.command]]
        if let Some(toml::Value::Array(cmds)) = keys.get("command") {
            self.consume("keys.command");
            for (i, c) in cmds.iter().enumerate() {
                self.push(&format!("keys.command[{i}]"), "keys.command", c.clone());
            }
        }
    }

    fn sidebar_tokens(&mut self) {
        if let Some(toml::Value::Array(rules)) = get(self.raw, "ui.sidebar.token").cloned() {
            self.consume("ui.sidebar.token");
            for (i, r) in rules.into_iter().enumerate() {
                self.push(&format!("ui.sidebar.token[{i}]"), "ui.sidebar.token", r);
            }
        }
    }
}

fn get<'a>(t: &'a toml::Table, path: &str) -> Option<&'a toml::Value> {
    let mut parts = path.split('.');
    let mut cur = t.get(parts.next()?)?;
    for p in parts {
        cur = cur.as_table()?.get(p)?;
    }
    Some(cur)
}

fn set_path(t: &mut toml::Table, path: &str, value: toml::Value) {
    let parts: Vec<&str> = path.split('.').collect();
    let mut cur = t;
    for p in &parts[..parts.len() - 1] {
        let e = cur
            .entry(p.to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if !e.is_table() {
            *e = toml::Value::Table(toml::Table::new());
        }
        cur = e.as_table_mut().unwrap();
    }
    cur.insert(parts[parts.len() - 1].to_string(), value);
}

fn push_path(t: &mut toml::Table, path: &str, value: toml::Value) {
    let parts: Vec<&str> = path.split('.').collect();
    let mut cur = t;
    for p in &parts[..parts.len() - 1] {
        let e = cur
            .entry(p.to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        cur = e.as_table_mut().unwrap();
    }
    let e = cur
        .entry(parts[parts.len() - 1].to_string())
        .or_insert_with(|| toml::Value::Array(vec![]));
    if let toml::Value::Array(a) = e {
        a.push(value);
    }
}

fn validate(t: &toml::Table) -> Result<(), String> {
    let text = toml::to_string(t).map_err(|e| e.to_string())?;
    match Config::parse(&text, Path::new("<import>")) {
        Ok(_) => Ok(()),
        Err(e) => Err(match e.first_diagnostic() {
            Some(d) => d.message.clone(),
            None => e.to_string(),
        }),
    }
}

fn strip_defaults(t: &mut toml::Table, defaults: &toml::Table) {
    let keys: Vec<String> = t.keys().cloned().collect();
    for k in keys {
        let Some(d) = defaults.get(&k) else { continue };
        let remove = match (t.get_mut(&k), d) {
            (Some(toml::Value::Table(sub)), toml::Value::Table(dsub)) => {
                strip_defaults(sub, dsub);
                sub.is_empty()
            }
            (Some(v), d) => v == d,
            _ => false,
        };
        if remove {
            t.remove(&k);
        }
    }
}

fn hint(path: &str) -> Option<&'static str> {
    Some(match path {
        p if p.starts_with("ui.sidebar.agents") || p.starts_with("ui.sidebar.spaces") => {
            "sidebar row layouts have no Vibeke equivalent yet"
        }
        p if p.starts_with("remote.manage_ssh_config") => "Vibeke manages SSH transport itself",
        p if p.starts_with("advanced.scrollback_limit_bytes") => {
            "Vibeke sizes scrollback in lines (terminal.scrollback_lines)"
        }
        p if p.starts_with("experimental") => "experimental Herdr option",
        p if p.starts_with("ui.sound.") => "per-event and per-agent sounds are not supported",
        p if p.starts_with("ui.toast.") => "toast placement is not configurable",
        _ => return None,
    })
}

fn collect_unsupported(
    raw: &toml::Table,
    prefix: &str,
    consumed: &BTreeSet<String>,
    out: &mut Vec<String>,
) {
    for (k, v) in raw {
        let path = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix}.{k}")
        };
        if consumed.contains(&path) {
            continue;
        }
        match v {
            toml::Value::Table(t) if hint(&path).is_none() && !t.is_empty() => {
                collect_unsupported(t, &path, consumed, out)
            }
            _ => out.push(match hint(&path) {
                Some(h) => format!("{path}: {h}"),
                None => path,
            }),
        }
    }
}

/// Where an imported config was written.
pub fn write_imported(dir: &Path, toml: &str, force: bool) -> io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let main = dir.join("config.toml");
    let target = if main.exists() && !force {
        dir.join("config.imported.toml")
    } else {
        main
    };
    let tmp = dir.join(format!(".{}.tmp", std::process::id()));
    std::fs::write(&tmp, toml)?;
    std::fs::rename(&tmp, &target)?;
    Ok(target)
}
