//! Config layers (08 §11, 07 §2.14 `config.get` sources), lowest to highest:
//!
//! 1. `default` — the built-in defaults;
//! 2. `user` — `config.toml`;
//! 3. `repo` — a **trusted** repository's `.vibeke/config.toml`: its `[tasks]` and `[preview]`
//!    keys override the user's, its `[[policy.rule]]` (deny/ask only: tighten-only) come after
//!    the user's rules and its `[[keys.command]]` are appended (`crate::repo`);
//! 4. `runtime` — `config.set` without `persist` (`crate::edit`);
//! 5. `cli` — per-invocation overrides: `--config-override key=value` (repeatable) and
//!    `VIBEKE_CONFIG_OVERRIDE="key=value;key=value"` (`;` or newlines between pairs). A value is
//!    parsed as a TOML value (`true`, `20`, `"x"`, `[1, 2]`) and taken as a plain string when it
//!    is not one, so `theme.name=dracula` works without quotes.
//!
//! The CLI layer is process-wide, read from the environment on first use; the CLI adds its
//! flags with [`add_cli_override`] and exports the merged set to the environment so a server
//! it starts inherits them (and so does everything that inherits the server's environment).
//! Like runtime overrides, CLI overrides apply when `config.toml` itself is loaded.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use crate::load::ConfigError;
use crate::repo::RepoConfig;
use crate::types::Config;

/// Environment variable carrying CLI-layer overrides.
pub const CONFIG_OVERRIDE_ENV: &str = "VIBEKE_CONFIG_OVERRIDE";

fn cli_layer() -> &'static Mutex<BTreeMap<String, toml::Value>> {
    static L: OnceLock<Mutex<BTreeMap<String, toml::Value>>> = OnceLock::new();
    L.get_or_init(|| {
        let mut m = BTreeMap::new();
        if let Ok(v) = std::env::var(CONFIG_OVERRIDE_ENV)
            && let Ok(pairs) = parse_override_list(&v)
        {
            m.extend(pairs);
        }
        Mutex::new(m)
    })
}

/// Parse one `key=value` override.
pub fn parse_override(s: &str) -> Result<(String, toml::Value), String> {
    let (k, v) = s
        .split_once('=')
        .ok_or_else(|| format!("config override `{s}` is not key=value"))?;
    let key = k.trim();
    crate::edit::split_key(key)?;
    let raw = v.trim();
    let value = format!("v = {raw}")
        .parse::<toml::Table>()
        .ok()
        .and_then(|mut t| t.remove("v"))
        .unwrap_or_else(|| toml::Value::String(raw.to_string()));
    Ok((key.to_string(), value))
}

/// Parse `key=value` pairs separated by `;` or newlines (empty entries are skipped).
pub fn parse_override_list(s: &str) -> Result<Vec<(String, toml::Value)>, String> {
    s.split([';', '\n'])
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(parse_override)
        .collect()
}

/// Add a CLI-layer override for this process (`--config-override`).
pub fn add_cli_override(key: &str, value: toml::Value) {
    cli_layer().lock().unwrap().insert(key.to_string(), value);
}

/// The CLI-layer overrides of this process.
pub fn cli_overrides() -> BTreeMap<String, toml::Value> {
    cli_layer().lock().unwrap().clone()
}

/// The CLI layer as `VIBEKE_CONFIG_OVERRIDE` text (for child processes).
pub fn cli_overrides_env() -> String {
    cli_overrides()
        .iter()
        .map(|(k, v)| format!("{k}={}", inline(v)))
        .collect::<Vec<_>>()
        .join(";")
}

fn inline(v: &toml::Value) -> String {
    match v {
        // A bare string round-trips through the string fallback unless it contains a separator.
        toml::Value::String(s) if !s.contains([';', '\n', '"']) => format!("{s:?}"),
        other => {
            let mut t = toml::Table::new();
            t.insert("v".into(), other.clone());
            toml::to_string(&t)
                .ok()
                .and_then(|s| s.strip_prefix("v = ").map(|x| x.trim().to_string()))
                .unwrap_or_default()
        }
    }
}

/// Whether any runtime or CLI override is in force.
pub(crate) fn has_overrides() -> bool {
    !crate::edit::runtime_overrides().is_empty() || !cli_overrides().is_empty()
}

/// Apply the CLI layer to config text (after the runtime layer).
pub(crate) fn apply_cli(doc: &mut toml_edit::DocumentMut) {
    for (k, v) in cli_overrides() {
        let _ = crate::edit::set_in_doc(doc, &k, Some(&v));
    }
}

/// Leaf keys of a table, dotted (`tasks.port_block`), with their values. Arrays and inline
/// values are leaves.
fn leaves(prefix: &str, t: &toml::Table, out: &mut Vec<(String, toml::Value)>) {
    for (k, v) in t {
        let key = if k.contains('.') || k.contains('"') {
            format!("{prefix}.\"{k}\"")
        } else {
            format!("{prefix}.{k}")
        };
        match v {
            toml::Value::Table(sub) => leaves(&key, sub, out),
            other => out.push((key, other.clone())),
        }
    }
}

/// The dotted keys a repo config sets (its `[tasks]` and `[preview]` leaves).
pub fn repo_keys(repo: &RepoConfig) -> Vec<(String, toml::Value)> {
    let mut out = vec![];
    for (name, t) in [("tasks", &repo.tasks), ("preview", &repo.preview)] {
        if let Some(t) = t {
            leaves(name, t, &mut out);
        }
    }
    out
}

impl Config {
    /// Load `path` with every layer in order: user file, the (already trusted) `repo` config,
    /// runtime overrides, CLI overrides. Repo policy rules and key commands are appended after
    /// the user's. Runtime and CLI layers apply only when `path` is the user's `config.toml`.
    pub fn load_layered(
        path: &Path,
        repo: Option<&RepoConfig>,
    ) -> Result<(Config, Vec<crate::load::Warning>), ConfigError> {
        let src = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => {
                return Err(ConfigError::Io {
                    path: path.to_path_buf(),
                    source: e,
                });
            }
        };
        let Ok(mut doc) = src.parse::<toml_edit::DocumentMut>() else {
            // Let the parser report the located error.
            return Config::parse(&src, path);
        };
        if let Some(r) = repo {
            for (k, v) in repo_keys(r) {
                let _ = crate::edit::set_in_doc(&mut doc, &k, Some(&v));
            }
        }
        if path == crate::load::config_path() {
            for (k, v) in crate::edit::runtime_overrides() {
                let _ = crate::edit::set_in_doc(&mut doc, &k, v.as_ref());
            }
            apply_cli(&mut doc);
        }
        let (mut c, w) = Config::parse(&doc.to_string(), path)?;
        if let Some(r) = repo {
            c.policy.rule.extend(r.policy_rules.iter().cloned());
            c.keys.command.extend(r.commands.iter().cloned());
        }
        Ok((c, w))
    }
}

/// `vibeke config reset-keys`: `src` with the `[keys]` table reset to the defaults. Key
/// commands (`[[keys.command]]`) are kept unless `all`. Returns the new text and the removed
/// keys (dotted).
pub fn reset_keys_text(src: &str, all: bool) -> Result<(String, Vec<String>), String> {
    let mut doc: toml_edit::DocumentMut = src.parse().map_err(|e| format!("{e}"))?;
    let mut removed = vec![];
    let Some(item) = doc.get_mut("keys") else {
        return Ok((doc.to_string(), removed));
    };
    let Some(t) = item.as_table_like_mut() else {
        return Err("`keys` is not a table".into());
    };
    let names: Vec<String> = t.iter().map(|(k, _)| k.to_string()).collect();
    for k in names {
        if k == "command" && !all {
            continue;
        }
        t.remove(&k);
        removed.push(format!("keys.{k}"));
    }
    if t.is_empty() {
        doc.remove("keys");
    }
    Ok((doc.to_string(), removed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overrides_parse_toml_values_with_a_string_fallback() {
        assert_eq!(
            parse_override("ui.animate=false").unwrap(),
            ("ui.animate".into(), toml::Value::Boolean(false))
        );
        assert_eq!(
            parse_override("tasks.port_block = 20").unwrap().1,
            toml::Value::Integer(20)
        );
        assert_eq!(
            parse_override("theme.name=dracula").unwrap().1,
            toml::Value::String("dracula".into())
        );
        assert_eq!(
            parse_override("theme.name=\"a b\"").unwrap().1,
            toml::Value::String("a b".into())
        );
        assert!(parse_override("novalue").is_err());
        let l = parse_override_list("a.b=1; c.d=x\n\n").unwrap();
        assert_eq!(l.len(), 2);
        // Round trip through the env text.
        let text = format!("{}={}", "x.y", inline(&toml::Value::String("hi".into())));
        assert_eq!(
            parse_override(&text).unwrap().1,
            toml::Value::String("hi".into())
        );
    }

    #[test]
    fn repo_layer_sits_between_user_and_overrides() {
        let d = tempfile::tempdir().unwrap();
        let user = d.path().join("config.toml");
        std::fs::write(
            &user,
            "[tasks]\nport_block = 12\nbranch_template = \"u/{slug}\"\n",
        )
        .unwrap();
        let repo = crate::repo::parse(
            "[tasks]\nport_block = 30\n[[policy.rule]]\nmatch = { tool = \"Bash\" }\neffect = \"deny\"\n",
            d.path(),
        )
        .unwrap();
        assert_eq!(
            repo_keys(&repo),
            vec![("tasks.port_block".into(), toml::Value::Integer(30))]
        );
        let (c, _) = Config::load_layered(&user, Some(&repo)).unwrap();
        assert_eq!(c.tasks.port_block, 30, "repo over user");
        assert_eq!(c.tasks.branch_template, "u/{slug}", "user kept");
        assert_eq!(c.policy.rule.len(), 1);
        let (c, _) = Config::load_layered(&user, None).unwrap();
        assert_eq!(c.tasks.port_block, 12);
    }

    #[test]
    fn reset_keys_keeps_commands_unless_all() {
        let src = "# mine\n[keys]\nprefix = \"ctrl+a\"\nsplit_right = \"prefix+v\"\n[[keys.command]]\nkey = \"prefix+t\"\ncommand = \"make\"\n\n[ui]\nanimate = false\n";
        let (out, removed) = reset_keys_text(src, false).unwrap();
        assert!(out.contains("# mine") && out.contains("animate = false"));
        assert!(!out.contains("ctrl+a") && out.contains("command = \"make\""));
        assert_eq!(removed, vec!["keys.prefix", "keys.split_right"]);
        let (out, removed) = reset_keys_text(src, true).unwrap();
        assert!(!out.contains("make") && !out.contains("[keys"), "{out}");
        assert_eq!(removed.len(), 3);
        assert_eq!(reset_keys_text("[ui]\n", false).unwrap().1.len(), 0);
    }
}
