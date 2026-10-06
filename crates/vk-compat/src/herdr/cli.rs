//! Grammar of the `herdr`-compatible CLI shim (`vibeke compat herdr …`, or the private `herdr`
//! launcher, 07 §8.2).
//!
//! Covers the commands the plugin corpus calls (`plugin install|link|unlink|uninstall|enable|
//! disable|list|config-dir|action|log|pane`, `pane …`, `workspace …`, `tab …`, `agent …`,
//! `worktree …`, `notification show`, `server reload-config`, `api schema`, `--version`) with a
//! regular mapping: `herdr <noun> <verb> [positionals] [--flag value]` becomes the socket method
//! `<noun>.<verb_with_underscores>` with positionals bound to the verb's parameter names and
//! flags as snake-case params. Flag spellings and positional order are *unverified* against
//! the baseline's CLI help snapshots (07 §8.0) and are pinned by the differential suite.

use serde_json::{Map, Value, json};

/// What one shim invocation does.
#[derive(Debug, Clone, PartialEq)]
pub enum Parsed {
    /// Send one request to the compat endpoint and print its result.
    Call {
        method: String,
        params: Value,
    },
    /// Operate on the per-user plugin registry (works without a server).
    Local(Local),
    Version,
    Help(String),
    /// Bad arguments (exit 2).
    Usage(String),
    /// A baseline command Vibeke deliberately does not perform (exit 1 with the reason).
    Refused {
        command: String,
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Local {
    PluginInstall {
        source: String,
        git_ref: Option<String>,
        yes: bool,
    },
    PluginLink {
        path: String,
        yes: bool,
    },
    PluginUnlink {
        id: String,
    },
    PluginUninstall {
        id: String,
    },
    PluginEnable {
        id: String,
    },
    PluginDisable {
        id: String,
    },
    PluginList,
    PluginConfigDir {
        id: String,
    },
}

/// `(noun, verb) → (method, positional parameter names)`. A trailing `*` name collects the rest
/// of the positionals into an array.
const VERBS: &[(&str, &str, &str, &[&str])] = &[
    ("session", "snapshot", "session.snapshot", &[]),
    ("workspace", "list", "workspace.list", &[]),
    ("workspace", "create", "workspace.create", &[]),
    ("workspace", "get", "workspace.get", &["workspace_id"]),
    (
        "workspace",
        "rename",
        "workspace.rename",
        &["workspace_id", "label"],
    ),
    (
        "workspace",
        "move",
        "workspace.move",
        &["workspace_id", "insert_index"],
    ),
    ("workspace", "focus", "workspace.focus", &["workspace_id"]),
    ("workspace", "close", "workspace.close", &["workspace_id"]),
    (
        "workspace",
        "report-metadata",
        "workspace.report_metadata",
        &["workspace_id"],
    ),
    ("tab", "list", "tab.list", &[]),
    ("tab", "create", "tab.create", &[]),
    ("tab", "rename", "tab.rename", &["tab_id", "label"]),
    ("tab", "move", "tab.move", &["tab_id", "insert_index"]),
    ("tab", "focus", "tab.focus", &["tab_id"]),
    ("tab", "close", "tab.close", &["tab_id"]),
    ("pane", "list", "pane.list", &[]),
    ("pane", "get", "pane.get", &["pane_id"]),
    ("pane", "current", "pane.current", &[]),
    ("pane", "read", "pane.read", &["pane_id"]),
    ("pane", "send-text", "pane.send_text", &["pane_id", "text"]),
    ("pane", "send-keys", "pane.send_keys", &["pane_id", "keys*"]),
    (
        "pane",
        "send-input",
        "pane.send_input",
        &["pane_id", "text"],
    ),
    ("pane", "run", "pane.run", &["pane_id", "command"]),
    ("pane", "focus", "pane.focus", &["pane_id"]),
    ("pane", "rename", "pane.rename", &["pane_id", "label"]),
    ("pane", "close", "pane.close", &["pane_id"]),
    ("pane", "split", "pane.split", &["pane_id"]),
    ("pane", "wait-output", "pane.wait_for_output", &["pane_id"]),
    ("pane", "report-agent", "pane.report_agent", &["pane_id"]),
    (
        "pane",
        "report-agent-session",
        "pane.report_agent_session",
        &["pane_id"],
    ),
    (
        "pane",
        "report-metadata",
        "pane.report_metadata",
        &["pane_id"],
    ),
    ("pane", "process-info", "pane.process_info", &["pane_id"]),
    ("pane", "move", "pane.move", &["pane_id"]),
    ("pane", "swap", "pane.swap", &["pane_id", "other_pane_id"]),
    ("pane", "resize", "pane.resize", &["pane_id"]),
    ("pane", "zoom", "pane.zoom", &["pane_id"]),
    ("agent", "list", "agent.list", &[]),
    ("agent", "get", "agent.get", &["target"]),
    ("agent", "start", "agent.start", &["agent"]),
    ("agent", "prompt", "agent.prompt", &["target", "text"]),
    ("agent", "wait", "agent.wait", &["target"]),
    ("agent", "read", "agent.read", &["target"]),
    ("agent", "rename", "agent.rename", &["target", "name"]),
    ("agent", "send", "agent.send", &["target", "text"]),
    ("worktree", "list", "worktree.list", &[]),
    ("worktree", "create", "worktree.create", &["branch"]),
    ("worktree", "open", "worktree.open", &["path"]),
    ("worktree", "repo-root", "worktree.repo_root", &[]),
    (
        "notification",
        "show",
        "notification.show",
        &["title", "body"],
    ),
    ("events", "subscribe", "events.subscribe", &[]),
    ("events", "wait", "events.wait", &[]),
    ("layout", "export", "layout.export", &[]),
    ("layout", "apply", "layout.apply", &[]),
    (
        "layout",
        "set-split-ratio",
        "layout.set_split_ratio",
        &["split_id", "ratio"],
    ),
    ("server", "reload-config", "server.reload_config", &[]),
    ("server", "stop", "server.stop", &[]),
    ("popup", "close", "popup.close", &[]),
    ("api", "schema", "api.schema", &[]),
];

/// Params whose values stay strings even when they look like numbers.
const TEXT_PARAMS: &[&str] = &[
    "text", "label", "title", "body", "name", "command", "keys", "branch", "path", "cwd",
    "message", "target", "agent", "pattern", "regex", "match",
];

const HELP: &str = "herdr (Vibeke compatibility shim, emulating Herdr 0.9.3 — partial)

usage: herdr <noun> <verb> [args] [--flag value]
nouns: session workspace tab pane agent worktree notification events layout server popup api plugin
plugin: install <path> [--yes] | link <path> | unlink <id> | uninstall <id> | enable <id> |
        disable <id> | list | config-dir <id> | action list | action invoke <plugin>.<action> |
        log list | pane open|focus|close
This shim talks to Vibeke, never to a running Herdr.";

fn coerce(key: &str, v: &str) -> Value {
    if TEXT_PARAMS.contains(&key) {
        return Value::String(v.to_string());
    }
    match v {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        _ => v
            .parse::<i64>()
            .map(Value::from)
            .ok()
            .or_else(|| {
                v.parse::<f64>()
                    .ok()
                    .filter(|f| f.is_finite())
                    .and_then(serde_json::Number::from_f64)
                    .map(Value::Number)
            })
            .unwrap_or_else(|| Value::String(v.to_string())),
    }
}

/// Split `args` into positionals and `--flag [value]` params.
fn split_flags(args: &[String]) -> Result<(Vec<String>, Map<String, Value>), String> {
    let mut pos = Vec::new();
    let mut flags = Map::new();
    let mut i = 0;
    let mut rest_positional = false;
    while i < args.len() {
        let a = &args[i];
        if rest_positional || !a.starts_with("--") || a == "-" {
            pos.push(a.clone());
            i += 1;
            continue;
        }
        if a == "--" {
            rest_positional = true;
            i += 1;
            continue;
        }
        let body = &a[2..];
        let (k, v) = match body.split_once('=') {
            Some((k, v)) => (k.replace('-', "_"), Some(v.to_string())),
            None => {
                let k = body.replace('-', "_");
                match args.get(i + 1) {
                    Some(n) if !n.starts_with("--") => {
                        i += 1;
                        (k, Some(n.clone()))
                    }
                    _ => (k, None),
                }
            }
        };
        if k.is_empty() {
            return Err(format!("bad flag `{a}`"));
        }
        let val = match v {
            Some(v) => coerce(&k, &v),
            None => Value::Bool(true),
        };
        flags.insert(k, val);
        i += 1;
    }
    Ok((pos, flags))
}

fn bind(method: &str, names: &[&str], args: &[String]) -> Parsed {
    let (pos, mut params) = match split_flags(args) {
        Ok(x) => x,
        Err(e) => return Parsed::Usage(e),
    };
    let mut it = pos.into_iter();
    for n in names {
        if let Some(rest) = n.strip_suffix('*') {
            let all: Vec<Value> = it.by_ref().map(Value::String).collect();
            if !all.is_empty() {
                params.insert(rest.to_string(), Value::Array(all));
            }
            break;
        }
        match it.next() {
            Some(v) => {
                params.insert(n.to_string(), coerce(n, &v));
            }
            None => break,
        }
    }
    let extra: Vec<String> = it.collect();
    if !extra.is_empty() {
        return Parsed::Usage(format!(
            "{method}: unexpected argument(s): {}",
            extra.join(" ")
        ));
    }
    // `--json` selects JSON output, which is the shim's only output format.
    params.remove("json");
    Parsed::Call {
        method: method.to_string(),
        params: Value::Object(params),
    }
}

fn one(args: &[String], what: &str) -> Result<String, Parsed> {
    let (pos, _) = split_flags(args).map_err(Parsed::Usage)?;
    match pos.as_slice() {
        [x] => Ok(x.clone()),
        _ => Err(Parsed::Usage(format!("herdr plugin {what} <id>"))),
    }
}

fn plugin(args: &[String]) -> Parsed {
    let verb = args.first().map(String::as_str).unwrap_or("");
    let rest = args.get(1..).unwrap_or(&[]);
    let local = |r: Result<String, Parsed>, f: fn(String) -> Local| match r {
        Ok(id) => Parsed::Local(f(id)),
        Err(p) => p,
    };
    match verb {
        "install" => match split_flags(rest) {
            Ok((pos, f)) if pos.len() == 1 => Parsed::Local(Local::PluginInstall {
                source: pos[0].clone(),
                git_ref: f.get("ref").and_then(|v| v.as_str()).map(str::to_string),
                yes: f.get("yes").and_then(Value::as_bool).unwrap_or(false),
            }),
            _ => Parsed::Usage(
                "herdr plugin install <owner/repo[/subdir] | path> [--ref R] [--yes]".into(),
            ),
        },
        "link" => match split_flags(rest) {
            Ok((pos, f)) if pos.len() == 1 => Parsed::Local(Local::PluginLink {
                path: pos[0].clone(),
                yes: f.get("yes").and_then(Value::as_bool).unwrap_or(false),
            }),
            _ => Parsed::Usage("herdr plugin link <path>".into()),
        },
        "unlink" => local(one(rest, "unlink"), |id| Local::PluginUnlink { id }),
        "uninstall" | "remove" => local(one(rest, "uninstall"), |id| Local::PluginUninstall { id }),
        "enable" => local(one(rest, "enable"), |id| Local::PluginEnable { id }),
        "disable" => local(one(rest, "disable"), |id| Local::PluginDisable { id }),
        "config-dir" => local(one(rest, "config-dir"), |id| Local::PluginConfigDir { id }),
        "list" | "ls" => Parsed::Local(Local::PluginList),
        "action" => {
            let sub = rest.first().map(String::as_str).unwrap_or("");
            let more = rest.get(1..).unwrap_or(&[]);
            match sub {
                "list" | "ls" => bind("plugin.action.list", &["plugin_id"], more),
                "invoke" | "run" => invoke(more),
                "" => Parsed::Usage("herdr plugin action list | invoke <plugin>.<action>".into()),
                // `herdr plugin action <plugin>.<action>`
                _ => invoke(rest),
            }
        }
        "log" | "logs" => {
            let more = match rest.first().map(String::as_str) {
                Some("list" | "ls") => &rest[1..],
                _ => rest,
            };
            bind("plugin.log.list", &["plugin_id"], more)
        }
        "pane" => {
            let sub = rest.first().map(String::as_str).unwrap_or("");
            let more = rest.get(1..).unwrap_or(&[]);
            match sub {
                "open" => bind("plugin.pane.open", &["plugin_id", "pane"], more),
                "focus" => bind("plugin.pane.focus", &["plugin_id", "pane"], more),
                "close" => bind("plugin.pane.close", &["plugin_id", "pane"], more),
                _ => Parsed::Usage("herdr plugin pane open|focus|close <plugin> <pane>".into()),
            }
        }
        "update" | "upgrade" => Parsed::Refused {
            command: format!("plugin {verb}"),
            reason: "managed updates need a git source; reinstall from the updated path".into(),
        },
        _ => Parsed::Usage(HELP.into()),
    }
}

/// `invoke <plugin>.<action> | <plugin> <action>`.
fn invoke(args: &[String]) -> Parsed {
    let (pos, mut f) = match split_flags(args) {
        Ok(x) => x,
        Err(e) => return Parsed::Usage(e),
    };
    match pos.as_slice() {
        [q] => {
            f.insert("action".into(), Value::String(q.clone()));
        }
        [p, a] => {
            f.insert("plugin_id".into(), Value::String(p.clone()));
            f.insert("action_id".into(), Value::String(a.clone()));
        }
        _ => return Parsed::Usage("herdr plugin action invoke <plugin>.<action>".into()),
    }
    f.remove("json");
    Parsed::Call {
        method: "plugin.action.invoke".into(),
        params: Value::Object(f),
    }
}

/// Remove Herdr's global `--session NAME` / `--session=NAME` flag from a command line (it may
/// appear anywhere before a `--` separator). Returns the selected session, if any, and the
/// remaining arguments; an empty or path-like name is a usage error.
pub fn take_session(args: &[String]) -> Result<(Option<String>, Vec<String>), String> {
    let mut out = Vec::with_capacity(args.len());
    let mut session = None;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            out.extend(args[i..].iter().cloned());
            break;
        }
        let v = if a == "--session" {
            i += 1;
            Some(
                args.get(i)
                    .cloned()
                    .ok_or("--session needs a session name")?,
            )
        } else {
            a.strip_prefix("--session=").map(str::to_string)
        };
        match v {
            Some(name) => {
                if !super::valid_session_name(&name) {
                    return Err(format!("invalid session name `{name}`"));
                }
                session = Some(name);
            }
            None => out.push(a.clone()),
        }
        i += 1;
    }
    Ok((session, out))
}

/// Parse a shim command line (without the program name).
pub fn parse(args: &[String]) -> Parsed {
    let noun = args.first().map(String::as_str).unwrap_or("");
    match noun {
        "" | "help" | "--help" | "-h" => return Parsed::Help(HELP.into()),
        "--version" | "-V" | "version" => return Parsed::Version,
        "ping" => {
            return Parsed::Call {
                method: "ping".into(),
                params: json!({}),
            };
        }
        "plugin" | "plugins" => return plugin(&args[1..]),
        "integration" | "integrations" => {
            return Parsed::Refused {
                command: format!("integration {}", args.get(1).map(String::as_str).unwrap_or("")),
                reason: "the shim never installs into Herdr or harness configs; use `vibeke integration install <harness>`".into(),
            };
        }
        "update" | "upgrade" | "web" | "completion" | "config" => {
            return Parsed::Refused {
                command: noun.to_string(),
                reason: "not part of Vibeke's Herdr emulation; use the vibeke command".into(),
            };
        }
        _ => {}
    }
    let verb = args.get(1).map(String::as_str).unwrap_or("");
    match VERBS.iter().find(|(n, v, _, _)| *n == noun && *v == verb) {
        Some((_, _, method, names)) => bind(method, names, &args[2..]),
        None if VERBS.iter().any(|(n, ..)| *n == noun) => Parsed::Usage(format!(
            "herdr {noun}: unknown verb `{verb}` (one of: {})",
            VERBS
                .iter()
                .filter(|(n, ..)| *n == noun)
                .map(|(_, v, ..)| *v)
                .collect::<Vec<_>>()
                .join(", ")
        )),
        None => Parsed::Usage(format!("herdr: unknown command `{noun}`\n\n{HELP}")),
    }
}

/// Socket methods the shim grammar can produce (for the inventory).
pub fn shim_methods() -> Vec<&'static str> {
    let mut v: Vec<&str> = VERBS.iter().map(|(_, _, m, _)| *m).collect();
    v.extend([
        "ping",
        "plugin.action.list",
        "plugin.action.invoke",
        "plugin.log.list",
        "plugin.pane.open",
        "plugin.pane.focus",
        "plugin.pane.close",
    ]);
    v
}

/// CLI commands (`noun verb`) the shim accepts.
pub fn shim_commands() -> Vec<String> {
    let mut v: Vec<String> = VERBS
        .iter()
        .map(|(n, vb, ..)| format!("{n} {vb}"))
        .collect();
    v.extend(
        [
            "plugin install",
            "plugin link",
            "plugin unlink",
            "plugin uninstall",
            "plugin enable",
            "plugin disable",
            "plugin list",
            "plugin config-dir",
            "plugin action list",
            "plugin action invoke",
            "plugin log list",
            "plugin pane open",
            "plugin pane focus",
            "plugin pane close",
            "ping",
            "--version",
        ]
        .map(String::from),
    );
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Parsed {
        parse(&s.split_whitespace().map(String::from).collect::<Vec<_>>())
    }
    fn call(s: &str) -> (String, Value) {
        match p(s) {
            Parsed::Call { method, params } => (method, params),
            other => panic!("{s}: {other:?}"),
        }
    }

    #[test]
    fn socket_commands() {
        assert_eq!(call("pane list"), ("pane.list".into(), json!({})));
        assert_eq!(
            call("pane read w1:p2 --source recent --lines 40"),
            (
                "pane.read".into(),
                json!({"pane_id": "w1:p2", "source": "recent", "lines": 40})
            )
        );
        assert_eq!(
            call("pane send-keys w1:p2 Enter C-c"),
            (
                "pane.send_keys".into(),
                json!({"pane_id": "w1:p2", "keys": ["Enter", "C-c"]})
            )
        );
        assert_eq!(
            call("pane send-text w1:p2 42"),
            (
                "pane.send_text".into(),
                json!({"pane_id": "w1:p2", "text": "42"})
            ),
            "text stays a string"
        );
        assert_eq!(
            call("workspace create --cwd /tmp --focus --label x"),
            (
                "workspace.create".into(),
                json!({"cwd": "/tmp", "focus": true, "label": "x"})
            )
        );
        assert_eq!(
            call("pane split w1:p1 --direction=right --json"),
            (
                "pane.split".into(),
                json!({"pane_id": "w1:p1", "direction": "right"})
            )
        );
        assert_eq!(
            call("pane wait-output w1:p1 --match ok").0,
            "pane.wait_for_output"
        );
        assert_eq!(
            call("notification show Hi there").1,
            json!({"title": "Hi", "body": "there"})
        );
        assert_eq!(call("ping").0, "ping");
        assert_eq!(call("server reload-config").0, "server.reload_config");
    }

    #[test]
    fn plugin_commands() {
        assert_eq!(
            p("plugin install ./x --yes"),
            Parsed::Local(Local::PluginInstall {
                source: "./x".into(),
                git_ref: None,
                yes: true
            })
        );
        assert_eq!(
            p("plugin install o/r --ref v1"),
            Parsed::Local(Local::PluginInstall {
                source: "o/r".into(),
                git_ref: Some("v1".into()),
                yes: false
            })
        );
        assert_eq!(p("plugin list"), Parsed::Local(Local::PluginList));
        assert_eq!(
            p("plugin config-dir acme.x"),
            Parsed::Local(Local::PluginConfigDir {
                id: "acme.x".into()
            })
        );
        assert_eq!(
            call("plugin action invoke acme.x.open"),
            (
                "plugin.action.invoke".into(),
                json!({"action": "acme.x.open"})
            )
        );
        assert_eq!(
            call("plugin action acme.x open"),
            (
                "plugin.action.invoke".into(),
                json!({"plugin_id": "acme.x", "action_id": "open"})
            )
        );
        assert_eq!(call("plugin action list").0, "plugin.action.list");
        assert_eq!(
            call("plugin log list acme.x").1,
            json!({"plugin_id": "acme.x"})
        );
        assert_eq!(call("plugin logs").0, "plugin.log.list");
        assert!(matches!(p("plugin unlink"), Parsed::Usage(_)));
    }

    #[test]
    fn session_flag() {
        let a = |s: &str| s.split_whitespace().map(String::from).collect::<Vec<_>>();
        assert_eq!(
            take_session(&a("--session work pane list")).unwrap(),
            (Some("work".into()), a("pane list"))
        );
        assert_eq!(
            take_session(&a("pane list --session=w2")).unwrap(),
            (Some("w2".into()), a("pane list"))
        );
        assert_eq!(
            take_session(&a("pane send-text p -- --session x")).unwrap(),
            (None, a("pane send-text p -- --session x")),
            "after -- it is text"
        );
        assert!(take_session(&a("--session")).is_err());
        assert!(take_session(&a("--session ../x pane list")).is_err());
    }

    #[test]
    fn refusals_and_usage() {
        assert!(matches!(
            p("integration install claude"),
            Parsed::Refused { .. }
        ));
        assert!(matches!(p("update"), Parsed::Refused { .. }));
        assert!(matches!(p("pane fly"), Parsed::Usage(_)));
        assert!(matches!(p("bogus"), Parsed::Usage(_)));
        assert!(matches!(p("pane get a b"), Parsed::Usage(_)));
        assert_eq!(p("--version"), Parsed::Version);
        assert!(matches!(p(""), Parsed::Help(_)));
        assert!(shim_methods().contains(&"pane.send_text"));
        assert!(shim_commands().contains(&"plugin install".to_string()));
    }
}
