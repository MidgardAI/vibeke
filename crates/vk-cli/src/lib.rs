#![allow(clippy::result_large_err)]
//! `vibeke <noun> <verb> …` — the CLI mirrors the API (07 §5). Every noun is an API namespace and
//! every verb a method; `--flag value` pairs become params (dashes → underscores), with a few
//! positional arguments per verb. `vibeke <noun>` alone prints help and never executes.

pub mod client;

use anyhow::Result;
use client::{CallError, Client};
use serde_json::{Map, Value, json};
use std::io::IsTerminal;
use std::path::PathBuf;

pub const EXIT_OK: i32 = 0;
pub const EXIT_API: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_TIMEOUT: i32 = 3;
pub const EXIT_NO_SERVER: i32 = 4;
pub const EXIT_PERMISSION: i32 = 5;

#[derive(Debug, Clone, Default)]
pub struct Global {
    pub session: String,
    pub machine: Option<String>,
    pub socket: Option<PathBuf>,
    pub json: Option<bool>,
    pub quiet: bool,
    pub no_spawn: bool,
    pub timeout_ms: Option<u64>,
}

/// (noun, verb, method, positional param names, help)
pub const COMMANDS: &[(&str, &str, &str, &[&str], &str)] = &[
    (
        "server",
        "status",
        "server.status",
        &[],
        "server pid, version, panes, clients",
    ),
    (
        "server",
        "stop",
        "server.stop",
        &[],
        "stop the server (holders keep running unless --kill-panes)",
    ),
    (
        "server",
        "reload-config",
        "server.reload_config",
        &[],
        "reload config.toml",
    ),
    (
        "session",
        "snapshot",
        "session.snapshot",
        &[],
        "full session state",
    ),
    (
        "workspace",
        "list",
        "workspace.list",
        &[],
        "workspaces with agent summaries",
    ),
    ("workspace", "get", "workspace.get", &["workspace"], ""),
    (
        "workspace",
        "create",
        "workspace.create",
        &["cwd"],
        "--cwd DIR [--name N] [--focus]",
    ),
    (
        "workspace",
        "rename",
        "workspace.rename",
        &["workspace", "name"],
        "",
    ),
    ("workspace", "focus", "workspace.focus", &["workspace"], ""),
    ("workspace", "close", "workspace.close", &["workspace"], ""),
    ("tab", "list", "tab.list", &[], "[--workspace w]"),
    (
        "tab",
        "create",
        "tab.create",
        &[],
        "[--workspace w] [--cwd d] [--title t]",
    ),
    ("tab", "rename", "tab.rename", &["tab", "title"], ""),
    ("tab", "focus", "tab.focus", &["tab"], ""),
    ("tab", "close", "tab.close", &["tab"], ""),
    (
        "pane",
        "list",
        "pane.list",
        &[],
        "[--workspace w] [--tab t] [--has-agent]",
    ),
    ("pane", "get", "pane.get", &["pane"], ""),
    (
        "pane",
        "current",
        "pane.current",
        &[],
        "the calling pane (needs VIBEKE_PANE_TOKEN)",
    ),
    (
        "pane",
        "split",
        "pane.split",
        &["pane"],
        "[--direction right|down|left|up] [--cwd d] [--command c] [--focus]",
    ),
    (
        "pane",
        "focus",
        "pane.focus",
        &["pane"],
        "[--direction left|right|up|down]",
    ),
    ("pane", "close", "pane.close", &["pane"], ""),
    ("pane", "zoom", "pane.zoom", &["pane"], ""),
    (
        "pane",
        "resize",
        "pane.resize",
        &["pane"],
        "--direction d [--percent n]",
    ),
    ("pane", "rename", "pane.rename", &["pane", "title"], ""),
    (
        "pane",
        "send-text",
        "pane.send_text",
        &["pane", "text"],
        "[--paste auto|bracketed|raw]",
    ),
    (
        "pane",
        "send-keys",
        "pane.send_keys",
        &["pane", "keys..."],
        "keys use the key grammar: enter ctrl+c alt+x y",
    ),
    (
        "pane",
        "run",
        "pane.run",
        &["pane", "command"],
        "[--wait] [--timeout-ms n]",
    ),
    (
        "pane",
        "read",
        "pane.read",
        &["pane"],
        "[--source visible|recent|recent_unwrapped|scrollback] [--lines n]",
    ),
    (
        "pane",
        "wait-output",
        "pane.wait_output",
        &["pane", "match"],
        "[--regex r] [--timeout-ms n]",
    ),
    (
        "pane",
        "wait-idle",
        "pane.wait_idle",
        &["pane"],
        "[--quiet-ms n] [--timeout-ms n]",
    ),
    ("pane", "mark-unread", "pane.mark_unread", &["pane"], ""),
    ("pane", "pin", "pane.pin", &["pane"], ""),
    (
        "agent",
        "list",
        "agent.list",
        &[],
        "[--workspace w] [--harness h]",
    ),
    ("agent", "get", "agent.get", &["target"], ""),
    (
        "agent",
        "start",
        "agent.start",
        &["name"],
        "--harness claude|codex [--pane p] [--args a,b]",
    ),
    (
        "agent",
        "spawn",
        "agent.spawn",
        &["name"],
        "--harness h [--split-of p] [--prompt text] [--focus]",
    ),
    (
        "agent",
        "prompt",
        "agent.prompt",
        &["target", "text"],
        "[--wait] [--timeout-ms n]",
    ),
    (
        "agent",
        "wait",
        "agent.wait",
        &["target"],
        "[--until idle,done,needs_approval,...] [--timeout-ms n]",
    ),
    ("agent", "interrupt", "agent.interrupt", &["target"], ""),
    (
        "agent",
        "send-keys",
        "agent.send_keys",
        &["target", "keys..."],
        "",
    ),
    (
        "agent",
        "read",
        "agent.read",
        &["target"],
        "[--source visible|recent|transcript] [--lines n]",
    ),
    (
        "agent",
        "transcript",
        "agent.transcript",
        &["target"],
        "[--limit n]",
    ),
    ("agent", "rename", "agent.rename", &["target", "name"], ""),
    ("agent", "release", "agent.release", &["target"], ""),
    ("agent", "resume", "agent.resume", &["run"], "[--pane p]"),
    (
        "agent",
        "resumable",
        "agent.resumable",
        &[],
        "ended runs that can be resumed",
    ),
    ("agent", "harnesses", "agent.harnesses", &[], ""),
    (
        "interaction",
        "list",
        "interaction.list",
        &[],
        "[--status open]",
    ),
    (
        "interaction",
        "get",
        "interaction.get",
        &["interaction"],
        "",
    ),
    (
        "interaction",
        "answer",
        "interaction.answer",
        &["interaction"],
        "--allow | --deny | --allow-always | --choice q=o | --text t",
    ),
    (
        "interaction",
        "cancel",
        "interaction.cancel",
        &["interaction"],
        "",
    ),
    (
        "ask",
        "list",
        "interaction.list",
        &[],
        "alias of interaction list",
    ),
    (
        "ask",
        "answer",
        "interaction.answer",
        &["interaction"],
        "alias of interaction answer",
    ),
    ("notification", "list", "notification.list", &[], ""),
    (
        "notification",
        "send",
        "notification.send",
        &["title", "body"],
        "",
    ),
    (
        "events",
        "read",
        "events.read",
        &[],
        "[--after-seq n] [--types agent.*]",
    ),
    (
        "events",
        "wait",
        "events.wait",
        &[],
        "--types t [--timeout-ms n]",
    ),
    ("search", "query", "search.query", &["q"], "[--pane p]"),
    (
        "task",
        "new",
        "task.create",
        &["title"],
        "[--repo .] [--agent claude:name] [--base ref] [--root sibling]",
    ),
    ("task", "list", "task.list", &[], ""),
    (
        "policy",
        "trust",
        "policy.trust",
        &["path"],
        "trust a repo's .vibeke/ automation (setup scripts) at its current digest; prints the script",
    ),
    ("task", "get", "task.get", &["task"], ""),
    (
        "task",
        "track",
        "task.track",
        &[],
        "[--pane @current|--run r] [--turn N] [--title t] [--criterion text]... [--stop-at draft_pr] — track the agent's work (no send, no spawn)",
    ),
    (
        "task",
        "sources",
        "task.sources",
        &[],
        "[--pane p|--run r] — recent requests you can track",
    ),
    (
        "task",
        "show",
        "task.detail",
        &["task"],
        "intent, bindings, baseline, messages",
    ),
    (
        "task",
        "intent",
        "task.intent.get",
        &["task"],
        "[--revision N]",
    ),
    (
        "task",
        "edit",
        "task.intent.update",
        &["task"],
        "[--title t] [--add-criterion text] [--stop-at x] [--expected-revision N] — record-only",
    ),
    (
        "task",
        "bind",
        "task.bind",
        &["task"],
        "[--pane p|--run r] [--role implementation]",
    ),
    ("task", "unbind", "task.unbind", &["task"], "[--binding b]"),
    (
        "task",
        "set",
        "task.set",
        &["task"],
        "[--priority N] [--effort quick|minutes|deep|unknown]",
    ),
    (
        "task",
        "message",
        "task.message.prepare",
        &["task", "text"],
        "[--communicates-intent] — draft only",
    ),
    (
        "task",
        "send",
        "task.message.send",
        &["message"],
        "send a prepared message (refuses with zero bytes when unsafe)",
    ),
    (
        "task",
        "message-status",
        "task.message.get",
        &["message"],
        "",
    ),
    (
        "task",
        "operation",
        "task.operation.get",
        &["idempotency_key"],
        "look up a mutation receipt",
    ),
    (
        "task",
        "finish",
        "task.finish",
        &["task"],
        "[--remove-worktree] [--force]",
    ),
    ("worktree", "list", "worktree.list", &[], "[--cwd d]"),
    (
        "worktree",
        "remove",
        "worktree.remove",
        &["path"],
        "[--force] (async)",
    ),
    ("worktree", "repo-root", "worktree.repo_root", &["cwd"], ""),
    ("layout", "export", "layout.export", &["tab"], ""),
    ("blob", "put", "blob.put", &[], "--path file | --data-b64 …"),
    (
        "machine",
        "list",
        "machine.list",
        &[],
        "saved remote machines",
    ),
    (
        "machine",
        "add",
        "machine.add",
        &["label", "address"],
        "user@host",
    ),
    ("machine", "remove", "machine.remove", &["machine"], ""),
    ("machine", "connect", "machine.connect", &["machine"], ""),
    (
        "machine",
        "disconnect",
        "machine.disconnect",
        &["machine"],
        "",
    ),
    ("machine", "status", "machine.status", &["machine"], ""),
    ("api", "methods", "api.methods", &[], "list API methods"),
    ("client", "list", "client.list", &[], ""),
];

pub fn nouns() -> Vec<&'static str> {
    let mut v: Vec<&str> = COMMANDS.iter().map(|c| c.0).collect();
    v.dedup();
    v
}

pub fn noun_help(noun: &str) -> String {
    let mut s = format!("vibeke {noun} <verb>\n\n");
    for (n, verb, method, pos, help) in COMMANDS {
        if *n == noun {
            let pos: Vec<String> = pos.iter().map(|p| format!("[{p}]")).collect();
            s.push_str(&format!(
                "  {verb:<14} {:<28} {method:<22} {help}\n",
                pos.join(" ")
            ));
        }
    }
    s
}

/// Parse a scalar CLI value: true/false, integers, JSON objects/arrays, else string.
fn scalar(v: &str) -> Value {
    match v {
        "true" => return Value::Bool(true),
        "false" => return Value::Bool(false),
        _ => {}
    }
    if (v.starts_with('{') || v.starts_with('['))
        && let Ok(j) = serde_json::from_str(v)
    {
        return j;
    }
    if (!v.starts_with('0') || v == "0")
        && let Ok(n) = v.parse::<i64>()
    {
        return Value::from(n);
    }
    Value::String(v.to_string())
}

/// Build params from argv after `<noun> <verb>`.
pub fn build_params(positional: &[&str], args: &[String]) -> Result<Value, String> {
    let mut map = Map::new();
    let mut pos = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            pos.extend(args[i + 1..].iter().cloned());
            break;
        }
        if let Some(flag) = a.strip_prefix("--") {
            let (k, inline) = match flag.split_once('=') {
                Some((k, v)) => (k.to_string(), Some(v.to_string())),
                None => (flag.to_string(), None),
            };
            if let Some(neg) = k.strip_prefix("no-") {
                map.insert(neg.replace('-', "_"), Value::Bool(false));
                i += 1;
                continue;
            }
            let key = k.replace('-', "_");
            let value = match inline {
                Some(v) => scalar(&v),
                None if i + 1 < args.len() && !args[i + 1].starts_with("--") => {
                    i += 1;
                    scalar(&args[i])
                }
                None => Value::Bool(true),
            };
            // Repeated flags accumulate into arrays.
            match map.get_mut(&key) {
                Some(Value::Array(a)) => a.push(value),
                Some(prev) => {
                    let p = prev.take();
                    *prev = Value::Array(vec![p, value]);
                }
                None => {
                    map.insert(key, value);
                }
            }
        } else {
            pos.push(a.clone());
        }
        i += 1;
    }
    let mut pi = 0;
    for name in positional {
        if let Some(rest) = name.strip_suffix("...") {
            let rest_vals: Vec<Value> = pos[pi.min(pos.len())..]
                .iter()
                .map(|s| Value::String(s.clone()))
                .collect();
            if !rest_vals.is_empty() {
                map.insert(rest.to_string(), Value::Array(rest_vals));
            }
            pi = pos.len();
            break;
        }
        if pi < pos.len() {
            map.entry(name.to_string())
                .or_insert_with(|| scalar(&pos[pi]));
            pi += 1;
        }
    }
    if pi < pos.len() {
        return Err(format!("unexpected argument `{}`", pos[pi]));
    }
    Ok(Value::Object(map))
}

/// Verb-specific param massaging so the CLI reads naturally.
fn adjust(method: &str, p: &mut Value) {
    let o = p.as_object_mut().expect("object");
    // Values given positionally as "@current"-style or numbers stay strings for targets.
    for k in [
        "pane",
        "tab",
        "workspace",
        "target",
        "interaction",
        "task",
        "run",
        "name",
        "title",
        "text",
        "machine",
        "label",
    ] {
        if let Some(v) = o.get_mut(k)
            && (v.is_number() || v.is_boolean())
        {
            *v = Value::String(v.to_string());
        }
    }
    if o.get("current").and_then(Value::as_bool) == Some(true) {
        o.remove("current");
        o.insert("pane".into(), json!("@current"));
    }
    match method {
        "interaction.answer" => {
            for (flag, d) in [
                ("allow", "allow"),
                ("deny", "deny"),
                ("allow_always", "allow_always"),
            ] {
                if o.remove(flag).and_then(|v| v.as_bool()) == Some(true) {
                    o.insert("decision".into(), json!(d));
                }
            }
            if let Some(c) = o.remove("choice") {
                let items: Vec<Value> = match c {
                    Value::Array(a) => a,
                    v => vec![v],
                };
                let mut choices = Map::new();
                for it in items {
                    if let Some((q, opt)) = it.as_str().and_then(|s| s.split_once('=')) {
                        let e = choices.entry(q.to_string()).or_insert(json!([]));
                        e.as_array_mut().unwrap().push(json!(opt));
                    }
                }
                o.insert("choices".into(), Value::Object(choices));
            }
        }
        "agent.wait" => {
            if let Some(Value::String(u)) = o.get("until").cloned() {
                o.insert("until".into(), json!(u.split(',').collect::<Vec<_>>()));
            }
        }
        "agent.start" | "agent.spawn" => {
            if let Some(Value::String(a)) = o.get("args").cloned() {
                o.insert("args".into(), json!(a.split(',').collect::<Vec<_>>()));
            }
        }
        "task.create" => {
            if let Some(a) = o.remove("agent") {
                let items: Vec<Value> = match a {
                    Value::Array(a) => a,
                    v => vec![v],
                };
                let agents: Vec<Value> = items
                    .iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| match s.split_once(':') {
                        Some((h, n)) => json!({"harness": h, "name": n}),
                        None => json!({"harness": s}),
                    })
                    .collect();
                o.insert("agents".into(), json!(agents));
            }
            if !o.contains_key("repo") {
                o.insert(
                    "repo".into(),
                    json!(
                        std::env::current_dir()
                            .map(|d| d.to_string_lossy().into_owned())
                            .unwrap_or_default()
                    ),
                );
            }
        }
        "workspace.create" | "tab.create" | "pane.split" => {
            if let Some(Value::String(c)) = o.get("cwd").cloned() {
                let abs = std::fs::canonicalize(&c)
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or(c);
                o.insert("cwd".into(), json!(abs));
            } else if method == "workspace.create" {
                o.insert(
                    "cwd".into(),
                    json!(
                        std::env::current_dir()
                            .map(|d| d.to_string_lossy().into_owned())
                            .unwrap_or_default()
                    ),
                );
            }
        }
        "blob.put" => {
            if let Some(Value::String(path)) = o.get("path").cloned() {
                let abs = std::fs::canonicalize(&path)
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or(path);
                o.insert("path".into(), json!(abs));
            }
        }
        _ => {}
    }
}

pub fn exit_code_for(e: &CallError) -> i32 {
    match e {
        CallError::Rpc(r) if r.data.kind == "timeout" => EXIT_TIMEOUT,
        CallError::Rpc(r) if r.data.kind == "permission_denied" || r.data.kind == "untrusted" => {
            EXIT_PERMISSION
        }
        _ => EXIT_API,
    }
}

pub fn print_error(e: &CallError) {
    match e {
        CallError::Rpc(r) => eprintln!(
            "{}",
            json!({"error": {"kind": r.data.kind, "message": r.message, "details": r.data.details}})
        ),
        CallError::Io(err) => eprintln!(
            "{}",
            json!({"error": {"kind": "io", "message": format!("{err:#}")}})
        ),
    }
}

/// Human output for a few list results; everything else is pretty JSON.
pub fn pretty(method: &str, v: &Value) -> String {
    let rows = |key: &str| {
        v.get(key)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    match method {
        "workspace.list" => rows("workspaces")
            .iter()
            .map(|w| {
                format!(
                    "{:<5} {:<24} {}",
                    w["handle"].as_str().unwrap_or(""),
                    w["name"].as_str().unwrap_or(""),
                    w["root_path"].as_str().unwrap_or("")
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        "pane.list" => rows("panes")
            .iter()
            .map(|p| {
                let agent = p
                    .get("agent")
                    .filter(|a| !a.is_null())
                    .map(|a| {
                        format!(
                            "{} {}",
                            a["harness"].as_str().unwrap_or(""),
                            a["state"].as_str().unwrap_or("")
                        )
                    })
                    .unwrap_or_default();
                format!(
                    "{:<10} {:<20} {:<40} {}",
                    p["handle"].as_str().unwrap_or(""),
                    p["auto_title"].as_str().unwrap_or(""),
                    p["cwd"].as_str().unwrap_or(""),
                    agent
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        "agent.list" => rows("runs")
            .iter()
            .map(|r| {
                let src = r["execution"]["source"].as_str().unwrap_or("");
                let inferred = if src == "Structured" || src == "SelfReport" {
                    ""
                } else {
                    "~"
                };
                format!(
                    "{:<5} {:<10} {:<8} {:<13} {:<10} {}",
                    r["handle"].as_str().unwrap_or(""),
                    r["name"].as_str().unwrap_or("-"),
                    r["harness"].as_str().unwrap_or(""),
                    format!(
                        "{}{inferred}",
                        r["execution"]["value"]
                            .as_str()
                            .unwrap_or("")
                            .to_lowercase()
                    ),
                    r["pane_handle"].as_str().unwrap_or(""),
                    r["last_message"]
                        .as_str()
                        .unwrap_or("")
                        .chars()
                        .take(60)
                        .collect::<String>()
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        "interaction.list" => rows("interactions")
            .iter()
            .map(|i| {
                format!(
                    "{:<5} {:<10} {:<10} {}",
                    i["handle"].as_str().unwrap_or(""),
                    i["kind"].as_str().unwrap_or(""),
                    i["pane_handle"].as_str().unwrap_or(""),
                    i["title"].as_str().unwrap_or("")
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        "pane.read" | "agent.read" => v["text"].as_str().unwrap_or("").to_string(),
        _ => serde_json::to_string_pretty(v).unwrap_or_default(),
    }
}

/// Run one API command. Returns the process exit code.
pub async fn run_api<S>(client: &mut Client<S>, g: &Global, method: &str, mut params: Value) -> i32
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    adjust(method, &mut params);
    if let Err(e) = client.hello("cli").await {
        print_error(&e);
        return exit_code_for(&e);
    }
    match client.call(method, params).await {
        Ok(v) => {
            if !g.quiet {
                let as_json = g.json.unwrap_or(!std::io::stdout().is_terminal());
                if as_json {
                    println!("{}", serde_json::to_string(&v).unwrap_or_default());
                } else {
                    println!("{}", pretty(method, &v));
                }
            }
            EXIT_OK
        }
        Err(e) => {
            print_error(&e);
            exit_code_for(&e)
        }
    }
}

/// Look up `(method, positional)` for `noun verb`.
pub fn lookup(noun: &str, verb: &str) -> Option<(&'static str, &'static [&'static str])> {
    COMMANDS
        .iter()
        .find(|c| c.0 == noun && c.1 == verb)
        .map(|c| (c.2, c.3))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_from_flags_and_positionals() {
        let args: Vec<String> = ["w1:p1", "--direction", "down", "--no-focus", "--ratio=0.3"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let p = build_params(&["pane"], &args).unwrap();
        assert_eq!(
            p,
            json!({"pane": "w1:p1", "direction": "down", "focus": false, "ratio": "0.3"})
        );
        let args: Vec<String> = ["w1:p1", "ctrl+c", "enter"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let p = build_params(&["pane", "keys..."], &args).unwrap();
        assert_eq!(p["keys"], json!(["ctrl+c", "enter"]));
        let mut p = build_params(&["interaction"], &["i3".into(), "--allow".into()]).unwrap();
        adjust("interaction.answer", &mut p);
        assert_eq!(p, json!({"interaction": "i3", "decision": "allow"}));
        assert!(build_params(&[], &["x".into()]).is_err());
    }

    #[test]
    fn every_command_has_a_unique_noun_verb() {
        let mut seen = std::collections::HashSet::new();
        for c in COMMANDS {
            assert!(seen.insert((c.0, c.1)), "duplicate {} {}", c.0, c.1);
        }
    }
}
