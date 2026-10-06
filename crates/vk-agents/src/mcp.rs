//! `vibeke integration install <harness> --mcp`: register `vibeke mcp` (spec 06 B7) as a stdio
//! MCP server in the harness's own config, with the same rules as the hook installers (04 §11):
//! merge-only, idempotent, backup before the first modification, atomic write with change
//! detection, and **no clobbering** — an existing `vibeke` entry that isn't ours is left alone.
//!
//! * **Claude Code**: user-scope MCP servers live in `~/.claude.json` under `mcpServers`
//!   (`$CLAUDE_CONFIG_DIR/.claude.json` when the config dir is redirected); `settings.json` has
//!   no server definitions. Entry: `{"type": "stdio", "command": <bin>, "args": ["mcp"],
//!   "env": {}}`. Claude passes its environment (incl. `VIBEKE_PANE_TOKEN`) to stdio servers.
//! * **Codex**: `<codex>/config.toml` table `[mcp_servers.vibeke]` with `command`, `args`,
//!   `env_vars` (Codex starts MCP servers with a filtered environment; these names are passed
//!   through) and a longer `tool_timeout_sec` for page loads. Edited with `toml_edit`, so
//!   comments and formatting elsewhere survive.
//!
//! Our entry is recognised by its shape (`args == ["mcp"]`, command named `vibeke`), not by a
//! marker key, so neither harness sees unknown keys.

use crate::install::{
    Dirs, FileChange, Harness, Plan, PlanKind, parse_root, read_file, render, resolve_symlink,
};
use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

/// The server name in the harness config.
pub const SERVER_NAME: &str = "vibeke";

/// Environment variables Codex should pass to `vibeke mcp`.
pub const CODEX_ENV_VARS: &[&str] = &[
    "VIBEKE_PANE_TOKEN",
    "VIBEKE_PANE_ID",
    "VIBEKE_SESSION",
    "VIBEKE_SOCKET",
    "VIBEKE_RUNTIME_DIR",
    "VIBEKE_STATE_DIR",
    "VIBEKE_CONFIG",
];

/// Where the MCP server entry goes.
pub fn mcp_config_file(h: Harness, dirs: &Dirs) -> Result<PathBuf> {
    match h {
        Harness::Claude => {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default();
            // Default layout keeps `.claude.json` next to `~/.claude`, in `$HOME`.
            if dirs.claude == home.join(".claude") {
                Ok(home.join(".claude.json"))
            } else {
                Ok(dirs.claude.join(".claude.json"))
            }
        }
        Harness::Codex => Ok(dirs.codex.join("config.toml")),
        other => bail!(
            "{} has no MCP client config; it gets Vibeke tools from its extension",
            other.id()
        ),
    }
}

fn is_ours(command: Option<&str>, args: &[String]) -> bool {
    args == ["mcp"]
        && command
            .and_then(|c| Path::new(c).file_name())
            .and_then(|n| n.to_str())
            .is_some_and(|n| n == "vibeke")
}

fn json_args(v: &Value) -> Vec<String> {
    v.get("args")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpStatus {
    pub file: PathBuf,
    /// Our entry is present.
    pub installed: bool,
    /// The command our entry runs.
    pub command: Option<String>,
    /// A `vibeke` entry exists that isn't ours (left alone).
    pub foreign: bool,
}

pub fn mcp_status(h: Harness, dirs: &Dirs) -> Result<McpStatus> {
    let file = resolve_symlink(&mcp_config_file(h, dirs)?);
    let text = read_file(&file)?.map(|(t, _)| t);
    let (installed, command, foreign) = match h {
        Harness::Codex => {
            let doc: toml_edit::DocumentMut = text
                .as_deref()
                .unwrap_or("")
                .parse()
                .map_err(|e| anyhow!("{} is not valid TOML: {e}", file.display()))?;
            match doc.get("mcp_servers").and_then(|t| t.get(SERVER_NAME)) {
                None => (false, None, false),
                Some(e) => {
                    let (cmd, args) = toml_entry(e);
                    let ours = is_ours(cmd.as_deref(), &args);
                    (ours, cmd, !ours)
                }
            }
        }
        _ => {
            let root = parse_root(&file, text.as_deref())?;
            match root.get("mcpServers").and_then(|m| m.get(SERVER_NAME)) {
                None => (false, None, false),
                Some(e) => {
                    let cmd = e.get("command").and_then(Value::as_str).map(str::to_string);
                    let ours = is_ours(cmd.as_deref(), &json_args(e));
                    (ours, cmd, !ours)
                }
            }
        }
    };
    Ok(McpStatus {
        file,
        installed,
        command,
        foreign,
    })
}

fn toml_entry(e: &toml_edit::Item) -> (Option<String>, Vec<String>) {
    let cmd = e
        .get("command")
        .and_then(|c| c.as_str())
        .map(str::to_string);
    let args = e
        .get("args")
        .and_then(|a| a.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    (cmd, args)
}

pub fn plan_mcp_install(h: Harness, dirs: &Dirs, vibeke_bin: &Path) -> Result<Plan> {
    plan(h, dirs, Some(vibeke_bin))
}

pub fn plan_mcp_uninstall(h: Harness, dirs: &Dirs) -> Result<Plan> {
    plan(h, dirs, None)
}

fn plan(h: Harness, dirs: &Dirs, bin: Option<&Path>) -> Result<Plan> {
    let path = resolve_symlink(&mcp_config_file(h, dirs)?);
    let existing = read_file(&path)?;
    let (text, stamp) = match &existing {
        Some((t, s)) => (Some(t.as_str()), Some(s.clone())),
        None => (None, None),
    };
    let mut notes = Vec::new();
    let after = match h {
        Harness::Codex => codex_after(&path, text, bin, &mut notes)?,
        _ => claude_after(&path, text, bin)?,
    };
    if bin.is_some() {
        notes.push(format!(
            "restart {} sessions to load the `{SERVER_NAME}` MCP server",
            h.id()
        ));
    }
    Ok(Plan {
        harness: h,
        kind: if bin.is_some() {
            PlanKind::Install
        } else {
            PlanKind::Uninstall
        },
        files: vec![FileChange {
            path,
            before: existing.map(|(t, _)| t),
            after,
            stamp,
            remove: false,
        }],
        notes,
    })
}

fn claude_after(path: &Path, text: Option<&str>, bin: Option<&Path>) -> Result<Option<String>> {
    let original = parse_root(path, text)?;
    let mut root = original.clone();
    let obj = root.as_object_mut().expect("checked object");
    match bin {
        Some(bin) => {
            let want = json!({
                "type": "stdio",
                "command": bin.display().to_string(),
                "args": ["mcp"],
                "env": {},
            });
            let servers = obj
                .entry("mcpServers")
                .or_insert_with(|| Value::Object(Map::new()))
                .as_object_mut()
                .ok_or_else(|| anyhow!("{}: \"mcpServers\" is not an object", path.display()))?;
            if let Some(cur) = servers.get(SERVER_NAME) {
                let cmd = cur.get("command").and_then(Value::as_str);
                if !is_ours(cmd, &json_args(cur)) {
                    bail!(
                        "{}: an MCP server named \"{SERVER_NAME}\" already exists and isn't Vibeke's; leaving it alone",
                        path.display()
                    );
                }
                // Keep user additions (e.g. env) on our entry; update command/args/type.
                let mut merged = cur.clone();
                for k in ["type", "command", "args"] {
                    merged[k] = want[k].clone();
                }
                servers.insert(SERVER_NAME.into(), merged);
            } else {
                servers.insert(SERVER_NAME.into(), want);
            }
        }
        None => {
            if text.is_none() {
                return Ok(None);
            }
            let Some(servers) = obj.get_mut("mcpServers").and_then(Value::as_object_mut) else {
                return Ok(text.map(str::to_string));
            };
            let ours = servers.get(SERVER_NAME).is_some_and(|cur| {
                is_ours(cur.get("command").and_then(Value::as_str), &json_args(cur))
            });
            if ours {
                servers.shift_remove(SERVER_NAME);
            }
        }
    }
    if root == original {
        // Nothing to do: keep the user's bytes (or create the file on install).
        return Ok(Some(match text {
            Some(t) => t.to_string(),
            None => render(&root, None),
        }));
    }
    Ok(Some(render(&root, text)))
}

fn codex_after(
    path: &Path,
    text: Option<&str>,
    bin: Option<&Path>,
    notes: &mut Vec<String>,
) -> Result<Option<String>> {
    use toml_edit::{Array, DocumentMut, Item, Table, value};
    if bin.is_none() && text.is_none() {
        return Ok(None);
    }
    let src = text.unwrap_or("");
    let mut doc: DocumentMut = src.parse().map_err(|e| {
        anyhow!(
            "{} is not valid TOML; refusing to modify it: {e}",
            path.display()
        )
    })?;
    match bin {
        Some(bin) => {
            if doc.get("mcp_servers").is_none() {
                let mut t = Table::new();
                t.set_implicit(true);
                doc.insert("mcp_servers", Item::Table(t));
            }
            let servers = doc["mcp_servers"]
                .as_table_like_mut()
                .ok_or_else(|| anyhow!("{}: mcp_servers is not a table", path.display()))?;
            if let Some(cur) = servers.get(SERVER_NAME) {
                let (cmd, args) = toml_entry(cur);
                if !is_ours(cmd.as_deref(), &args) {
                    bail!(
                        "{}: [mcp_servers.{SERVER_NAME}] already exists and isn't Vibeke's; leaving it alone",
                        path.display()
                    );
                }
            } else {
                servers.insert(SERVER_NAME, Item::Table(Table::new()));
            }
            let entry = servers
                .get_mut(SERVER_NAME)
                .and_then(|e| e.as_table_like_mut())
                .ok_or_else(|| anyhow!("mcp_servers.{SERVER_NAME} is not a table"))?;
            let want_cmd = bin.display().to_string();
            if entry.get("command").and_then(|c| c.as_str()) != Some(want_cmd.as_str()) {
                entry.insert("command", value(want_cmd));
            }
            let mut args = Array::new();
            args.push("mcp");
            if entry
                .get("args")
                .and_then(|a| a.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>())
                != Some(vec!["mcp"])
            {
                entry.insert("args", value(args));
            }
            let have: Vec<String> = entry
                .get("env_vars")
                .and_then(|a| a.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            if CODEX_ENV_VARS.iter().any(|v| !have.iter().any(|h| h == v)) {
                let mut vars = Array::new();
                for v in have.iter().map(String::as_str).chain(
                    CODEX_ENV_VARS
                        .iter()
                        .copied()
                        .filter(|v| !have.iter().any(|h| h == v)),
                ) {
                    vars.push(v);
                }
                entry.insert("env_vars", value(vars));
            }
            if entry.get("tool_timeout_sec").is_none() {
                entry.insert("tool_timeout_sec", value(120));
            }
            notes.push(
                "Codex passes only the listed env_vars to MCP servers; pane scope also works by process ancestry"
                    .to_string(),
            );
        }
        None => {
            let ours = doc
                .get("mcp_servers")
                .and_then(|t| t.get(SERVER_NAME))
                .is_some_and(|e| {
                    let (cmd, args) = toml_entry(e);
                    is_ours(cmd.as_deref(), &args)
                });
            if ours
                && let Some(t) = doc
                    .get_mut("mcp_servers")
                    .and_then(|t| t.as_table_like_mut())
            {
                t.remove(SERVER_NAME);
                if t.is_empty() {
                    doc.remove("mcp_servers");
                }
            }
        }
    }
    let out = doc.to_string();
    if out == src {
        return Ok(Some(src.to_string()));
    }
    Ok(Some(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::apply;
    use std::fs;

    const BIN: &str = "/home/u/.local/bin/vibeke";

    fn dirs(t: &tempfile::TempDir) -> Dirs {
        let d = Dirs {
            claude: t.path().join("claude"),
            codex: t.path().join("codex"),
            pi: t.path().join("pi"),
            omp: t.path().join("omp"),
            opencode: t.path().join("opencode"),
            gemini: t.path().join("gemini"),
        };
        fs::create_dir_all(&d.claude).unwrap();
        fs::create_dir_all(&d.codex).unwrap();
        d
    }

    fn install(h: Harness, d: &Dirs) -> Plan {
        let p = plan_mcp_install(h, d, Path::new(BIN)).unwrap();
        apply(&p).unwrap();
        p
    }

    #[test]
    fn claude_redirected_dir_gets_its_own_claude_json() {
        let t = tempfile::tempdir().unwrap();
        let d = dirs(&t);
        assert_eq!(
            mcp_config_file(Harness::Claude, &d).unwrap(),
            d.claude.join(".claude.json")
        );
        assert_eq!(
            mcp_config_file(Harness::Codex, &d).unwrap(),
            d.codex.join("config.toml")
        );
        assert!(mcp_config_file(Harness::Pi, &d).is_err());
    }

    #[test]
    fn claude_install_merges_and_is_idempotent() {
        let t = tempfile::tempdir().unwrap();
        let d = dirs(&t);
        let f = d.claude.join(".claude.json");
        let user = "{\n    \"numStartups\": 7,\n    \"mcpServers\": {\n        \"github\": {\"type\": \"stdio\", \"command\": \"gh-mcp\", \"args\": []}\n    },\n    \"projects\": {}\n}\n";
        fs::write(&f, user).unwrap();
        install(Harness::Claude, &d);
        let v: Value = serde_json::from_str(&fs::read_to_string(&f).unwrap()).unwrap();
        assert_eq!(v["numStartups"], 7);
        assert_eq!(v["mcpServers"]["github"]["command"], "gh-mcp");
        assert_eq!(v["mcpServers"]["vibeke"]["command"], BIN);
        assert_eq!(v["mcpServers"]["vibeke"]["args"], json!(["mcp"]));
        assert_eq!(v["mcpServers"]["vibeke"]["type"], "stdio");
        // 4-space indentation kept.
        assert!(
            fs::read_to_string(&f)
                .unwrap()
                .contains("\n    \"numStartups\"")
        );
        let bytes = fs::read(&f).unwrap();
        let again = plan_mcp_install(Harness::Claude, &d, Path::new(BIN)).unwrap();
        assert!(!again.changed());
        assert_eq!(fs::read(&f).unwrap(), bytes);
        // One backup of the user's original.
        let backups: Vec<_> = fs::read_dir(&d.claude)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains(".vibeke-bak-")
            })
            .collect();
        assert_eq!(backups.len(), 1);
        let st = mcp_status(Harness::Claude, &d).unwrap();
        assert!(st.installed && !st.foreign);
        // A moved binary updates our entry and keeps user additions to it.
        let mut v2 = v.clone();
        v2["mcpServers"]["vibeke"]["env"] = json!({"X": "1"});
        fs::write(&f, serde_json::to_string_pretty(&v2).unwrap()).unwrap();
        let p = plan_mcp_install(Harness::Claude, &d, Path::new("/opt/bin/vibeke")).unwrap();
        apply(&p).unwrap();
        let v3: Value = serde_json::from_str(&fs::read_to_string(&f).unwrap()).unwrap();
        assert_eq!(v3["mcpServers"]["vibeke"]["command"], "/opt/bin/vibeke");
        assert_eq!(v3["mcpServers"]["vibeke"]["env"]["X"], "1");
        // Uninstall removes only ours.
        apply(&plan_mcp_uninstall(Harness::Claude, &d).unwrap()).unwrap();
        let v4: Value = serde_json::from_str(&fs::read_to_string(&f).unwrap()).unwrap();
        assert!(v4["mcpServers"].get("vibeke").is_none());
        assert_eq!(v4["mcpServers"]["github"]["command"], "gh-mcp");
        assert_eq!(v4["numStartups"], 7);
    }

    #[test]
    fn claude_foreign_entry_is_never_clobbered() {
        let t = tempfile::tempdir().unwrap();
        let d = dirs(&t);
        let f = d.claude.join(".claude.json");
        let user =
            r#"{"mcpServers": {"vibeke": {"command": "npx", "args": ["some-other-vibeke"]}}}"#;
        fs::write(&f, user).unwrap();
        let e = plan_mcp_install(Harness::Claude, &d, Path::new(BIN)).unwrap_err();
        assert!(format!("{e:#}").contains("leaving it alone"));
        assert_eq!(fs::read_to_string(&f).unwrap(), user);
        let st = mcp_status(Harness::Claude, &d).unwrap();
        assert!(st.foreign && !st.installed);
        // Uninstall leaves a foreign entry untouched too.
        let p = plan_mcp_uninstall(Harness::Claude, &d).unwrap();
        assert!(!p.changed());
    }

    #[test]
    fn claude_missing_file_is_created_and_invalid_json_refused() {
        let t = tempfile::tempdir().unwrap();
        let d = dirs(&t);
        let f = d.claude.join(".claude.json");
        assert!(!plan_mcp_uninstall(Harness::Claude, &d).unwrap().changed());
        install(Harness::Claude, &d);
        let v: Value = serde_json::from_str(&fs::read_to_string(&f).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["vibeke"]["args"], json!(["mcp"]));
        fs::write(&f, "{not json").unwrap();
        assert!(plan_mcp_install(Harness::Claude, &d, Path::new(BIN)).is_err());
    }

    #[test]
    fn codex_config_toml_keeps_comments_and_other_servers() {
        let t = tempfile::tempdir().unwrap();
        let d = dirs(&t);
        let f = d.codex.join("config.toml");
        let user = "# my settings\nmodel = \"gpt-5\"\n\n[mcp_servers.docs]\ncommand = \"docs-mcp\" # keep me\nargs = [\"--fast\"]\n\n[features]\nhooks = true\n";
        fs::write(&f, user).unwrap();
        let p = install(Harness::Codex, &d);
        assert!(p.notes.iter().any(|n| n.contains("env_vars")));
        let text = fs::read_to_string(&f).unwrap();
        assert!(
            text.starts_with("# my settings\nmodel = \"gpt-5\""),
            "{text}"
        );
        assert!(text.contains("command = \"docs-mcp\" # keep me"), "{text}");
        let v: toml::Table = text.parse().unwrap();
        let e = &v["mcp_servers"]["vibeke"];
        assert_eq!(e["command"].as_str(), Some(BIN));
        assert_eq!(e["args"].as_array().unwrap()[0].as_str(), Some("mcp"));
        let vars: Vec<&str> = e["env_vars"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap())
            .collect();
        assert!(vars.contains(&"VIBEKE_PANE_TOKEN") && vars.contains(&"VIBEKE_SOCKET"));
        assert_eq!(e["tool_timeout_sec"].as_integer(), Some(120));
        assert_eq!(v["features"]["hooks"].as_bool(), Some(true));
        // Idempotent, byte for byte.
        assert!(
            !plan_mcp_install(Harness::Codex, &d, Path::new(BIN))
                .unwrap()
                .changed()
        );
        assert!(mcp_status(Harness::Codex, &d).unwrap().installed);
        // Uninstall removes our table only.
        apply(&plan_mcp_uninstall(Harness::Codex, &d).unwrap()).unwrap();
        let text = fs::read_to_string(&f).unwrap();
        let v: toml::Table = text.parse().unwrap();
        assert!(v["mcp_servers"].get("vibeke").is_none());
        assert_eq!(
            v["mcp_servers"]["docs"]["command"].as_str(),
            Some("docs-mcp")
        );
        assert!(text.contains("# my settings"));
    }

    #[test]
    fn codex_foreign_entry_and_fresh_file() {
        let t = tempfile::tempdir().unwrap();
        let d = dirs(&t);
        let f = d.codex.join("config.toml");
        // Fresh file.
        install(Harness::Codex, &d);
        let v: toml::Table = fs::read_to_string(&f).unwrap().parse().unwrap();
        assert_eq!(v["mcp_servers"]["vibeke"]["command"].as_str(), Some(BIN));
        // Foreign entry.
        let user = "[mcp_servers.vibeke]\ncommand = \"/usr/bin/other\"\nargs = []\n";
        fs::write(&f, user).unwrap();
        let e = plan_mcp_install(Harness::Codex, &d, Path::new(BIN)).unwrap_err();
        assert!(format!("{e:#}").contains("leaving it alone"));
        assert_eq!(fs::read_to_string(&f).unwrap(), user);
        assert!(!plan_mcp_uninstall(Harness::Codex, &d).unwrap().changed());
        fs::write(&f, "not = [toml").unwrap();
        assert!(plan_mcp_install(Harness::Codex, &d, Path::new(BIN)).is_err());
    }
}
