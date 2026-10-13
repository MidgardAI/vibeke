//! CLI verbs that are more than one API call (07 §5.3): `vibeke events tail [--follow]`,
//! `vibeke screenshot add <file>...`, `vibeke msg ...`, `vibeke name` and `vibeke completion <shell>`.

use crate::client::{CallError, Client};
use crate::{COMMANDS, EXIT_OK, EXIT_USAGE, Global, build_params, exit_code_for, print_error};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub const EVENTS_TAIL_USAGE: &str = "vibeke events tail [--types 'agent.*,pane.created'] [--after-seq N | --lines N] [--follow]\n  one JSON event per line; without --after-seq the last N (default 20) events are shown first";

pub const SCREENSHOT_ADD_USAGE: &str = "vibeke screenshot add <file>... [--caption TEXT] [--pane P] [--json]\n  attaches PNG or JPEG files so the user can see them in Vibeke on any device (the CLI reads the files; the server never opens your paths)";

pub const MSG_USAGE: &str = "vibeke msg @name [@name…] <text|-> [--no-reply] [--json]\n  vibeke msg reply <message-id> <text|-> [--json]\n  vibeke msg done <message-id>\n  vibeke msg inbox [--open] [--json]\n  vibeke msg list [--open] [--json]\n  sends a message to named panes of this workspace (`-` reads the text from stdin); bodies are untrusted input for the receiving agent";

pub const NAME_USAGE: &str = "vibeke name [@name] [--pane TARGET] [--clear] [--json]\n  gives a pane a mention name (without a name: the suggested one) so other panes can message it; --clear removes the name; the pane defaults to the current one";

/// Largest image file `screenshot.add` accepts (the server checks too). Its base64 must fit
/// one 16 MiB request line.
pub const ADD_MAX_BYTES: u64 = 11 << 20;

/// Read an image file for `screenshot.add`, refusing one larger than [`ADD_MAX_BYTES`] before
/// reading it (and while reading, in case it grows). The error is a message for the user.
pub fn read_image(path: &std::path::Path) -> Result<Vec<u8>, String> {
    use std::io::Read as _;
    let shown = path.display();
    let too_big = || format!("{shown}: image is larger than {} MiB", ADD_MAX_BYTES >> 20);
    let meta = std::fs::metadata(path).map_err(|e| format!("{shown}: {e}"))?;
    if meta.len() > ADD_MAX_BYTES {
        return Err(too_big());
    }
    let file = std::fs::File::open(path).map_err(|e| format!("{shown}: {e}"))?;
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    file.take(ADD_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{shown}: {e}"))?;
    if bytes.len() as u64 > ADD_MAX_BYTES {
        return Err(too_big());
    }
    Ok(bytes)
}

/// `vibeke screenshot add <file>...`: read each image file here, with the caller's own
/// permissions, and attach it with `screenshot.add`. Exit non-zero if any file fails.
pub async fn screenshot_add<S>(client: &mut Client<S>, g: &Global, args: &[String]) -> i32
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use base64::Engine as _;
    let p = match build_params(&["files..."], args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}\n{SCREENSHOT_ADD_USAGE}");
            return EXIT_USAGE;
        }
    };
    let files: Vec<String> = match p.get("files") {
        Some(Value::Array(a)) => a
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| v.to_string())
            })
            .collect(),
        _ => vec![],
    };
    if files.is_empty() {
        eprintln!("{SCREENSHOT_ADD_USAGE}");
        return EXIT_USAGE;
    }
    let text = |k: &str| match p.get(k) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Array(a)) => a.first().and_then(Value::as_str).map(str::to_string),
        _ => None,
    };
    let caption = text("caption");
    let pane = text("pane");
    let as_json = g.json == Some(true);
    if let Err(e) = client.hello("cli").await {
        print_error(&e);
        return exit_code_for(&e);
    }
    let mut code = EXIT_OK;
    for file in &files {
        let path = std::path::Path::new(file);
        let bytes = match read_image(path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("vibeke screenshot add: {e}");
                code = EXIT_USAGE;
                continue;
            }
        };
        let mut params = json!({
            "data_b64": base64::engine::general_purpose::STANDARD.encode(&bytes),
            "name": path.file_name().map(|n| n.to_string_lossy().into_owned()),
        });
        if let Some(c) = &caption {
            params["caption"] = json!(c);
        }
        if let Some(pn) = &pane {
            params["pane"] = json!(pn);
        }
        match client.call("screenshot.add", params).await {
            Ok(mut r) => {
                let handle = r["handle"].as_str().unwrap_or("").to_string();
                let dup = r["duplicate"].as_bool().unwrap_or(false);
                if as_json {
                    r["file"] = json!(file);
                    println!("{}", serde_json::to_string(&r).unwrap_or_default());
                } else {
                    let note = if dup {
                        "(already shown to the user)"
                    } else {
                        "(shown to the user)"
                    };
                    println!("{handle}  {file}  {note}");
                }
            }
            Err(e) => {
                eprint!("{file}: ");
                print_error(&e);
                code = exit_code_for(&e);
            }
        }
    }
    code
}

// ---- mentions (spec 19) ---------------------------------------------------------------------------

/// The text of a message: the given words joined by spaces, or all of stdin for `-`.
fn message_text(words: &[String]) -> Result<String, String> {
    use std::io::Read as _;
    let text = if words.len() == 1 && words[0] == "-" {
        let mut t = String::new();
        std::io::stdin()
            .read_to_string(&mut t)
            .map_err(|e| format!("cannot read stdin: {e}"))?;
        t.trim_end().to_string()
    } else {
        words.join(" ")
    };
    if text.trim().is_empty() {
        return Err("the message text is empty".into());
    }
    Ok(text)
}

/// `@name` or `name` as the API wants it (without the `@`).
fn bare_name(s: &str) -> &str {
    s.strip_prefix('@').unwrap_or(s)
}

fn first_line(body: &str, max: usize) -> String {
    let line = body.lines().next().unwrap_or("").trim();
    if line.chars().count() > max {
        let cut: String = line.chars().take(max).collect();
        format!("{cut}…")
    } else {
        line.to_string()
    }
}

/// "@coder", "you" or "?" for a message author.
fn author_label(from: &Value) -> String {
    match from["kind"].as_str() {
        Some("agent") => match from["name"].as_str() {
            Some(n) => format!("@{n}"),
            None => "an agent".into(),
        },
        Some("human") => "you".into(),
        _ => "?".into(),
    }
}

fn deliveries_summary(ds: &[Value]) -> String {
    ds.iter()
        .map(|d| {
            format!(
                "@{} ({})",
                d["name"].as_str().unwrap_or("?"),
                d["state"].as_str().unwrap_or("?")
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn print_message_rows(items: &[Value]) {
    if items.is_empty() {
        println!("no messages");
    }
    for it in items {
        let m = &it["message"];
        let ds = it["deliveries"].as_array().cloned().unwrap_or_default();
        println!(
            "{}  {} -> {}  {}\n    {}",
            m["id"].as_str().unwrap_or("?"),
            author_label(&m["from"]),
            if ds.is_empty() {
                m["to"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(|n| format!("@{n}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default()
            } else {
                deliveries_summary(&ds)
            },
            if m["in_reply_to"].is_null() {
                String::new()
            } else {
                format!("(reply to {})", m["in_reply_to"].as_str().unwrap_or("?"))
            },
            first_line(m["body"].as_str().unwrap_or(""), 100),
        );
    }
}

/// `vibeke msg @a [@b…] <text>`, `msg reply|done|inbox|list`.
pub async fn msg<S>(client: &mut Client<S>, g: &Global, args: &[String]) -> i32
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let as_json = g.json.unwrap_or(false);
    // Split flags from words; a lone `-` is the stdin marker and stays a word.
    let mut words: Vec<String> = Vec::new();
    let (mut no_reply, mut open_only) = (false, false);
    let mut workspace: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--no-reply" => no_reply = true,
            "--open" => open_only = true,
            "--workspace" if i + 1 < args.len() => {
                i += 1;
                workspace = Some(args[i].clone());
            }
            "--" => {
                words.extend(args[i + 1..].iter().cloned());
                break;
            }
            _ => words.push(args[i].clone()),
        }
        i += 1;
    }
    let usage = |why: &str| {
        eprintln!("{why}\n{MSG_USAGE}");
        EXIT_USAGE
    };
    enum Op {
        Send(Value),
        Reply(Value),
        Done(Value),
        List(Value),
    }
    let op = match words.first().map(String::as_str) {
        None => return usage("missing recipients or subcommand"),
        Some("reply") => {
            let (Some(id), rest) = (words.get(1), words.get(2..).unwrap_or(&[])) else {
                return usage("usage: vibeke msg reply <message-id> <text>");
            };
            match message_text(rest) {
                Ok(body) => Op::Reply(json!({"message": id, "body": body})),
                Err(e) => return usage(&e),
            }
        }
        Some("done") => match words.get(1) {
            Some(id) if words.len() == 2 => Op::Done(json!({"message": id})),
            _ => return usage("usage: vibeke msg done <message-id>"),
        },
        Some(v @ ("inbox" | "list")) => {
            if words.len() > 1 {
                return usage(&format!("unexpected argument `{}`", words[1]));
            }
            let mut p = json!({"direction": if v == "inbox" { "in" } else { "all" }});
            if open_only {
                p["open_only"] = json!(true);
            }
            Op::List(p)
        }
        Some(_) => {
            let n = words.iter().take_while(|w| w.starts_with('@')).count();
            if n == 0 {
                return usage(
                    "recipients start with @, for example: vibeke msg @reviewer \"please look\"",
                );
            }
            let to: Vec<&str> = words[..n].iter().map(|w| bare_name(w)).collect();
            let body = match message_text(&words[n..]) {
                Ok(b) => b,
                Err(e) => return usage(&e),
            };
            let mut p = json!({"to": to, "body": body});
            if no_reply {
                p["expects_reply"] = json!(false);
            }
            if let Some(w) = workspace {
                p["workspace"] = json!(w);
            }
            Op::Send(p)
        }
    };
    if let Err(e) = client.hello("cli").await {
        print_error(&e);
        return exit_code_for(&e);
    }
    let (method, params) = match &op {
        Op::Send(p) => ("message.send", p.clone()),
        Op::Reply(p) => ("message.reply", p.clone()),
        Op::Done(p) => ("message.complete", p.clone()),
        Op::List(p) => ("message.list", p.clone()),
    };
    let r = match client.call(method, params).await {
        Ok(r) => r,
        Err(e) => {
            print_error(&e);
            return exit_code_for(&e);
        }
    };
    if as_json {
        println!("{}", serde_json::to_string(&r).unwrap_or_default());
        return EXIT_OK;
    }
    match op {
        Op::Send(_) | Op::Reply(_) => {
            let ds = r["deliveries"].as_array().cloned().unwrap_or_default();
            println!(
                "{} sent to {}",
                r["message"]["id"].as_str().unwrap_or("?"),
                deliveries_summary(&ds)
            );
            // Agents read this: a reply comes back on its own (spec 19 §4.5).
            let expects_reply = ds.iter().any(|d| d["expects_reply"] == true);
            if matches!(op, Op::Send(_)) && expects_reply {
                println!(
                    "The reply reaches this pane on its own, during or after your turn. Do not poll for it."
                );
            }
        }
        Op::Done(p) => println!("{} marked as done", p["message"].as_str().unwrap_or("?")),
        Op::List(_) => {
            let items = r["items"].as_array().cloned().unwrap_or_default();
            print_message_rows(&items);
        }
    }
    EXIT_OK
}

/// `vibeke name [@name] [--pane TARGET] [--clear]`: name a pane (the suggested name when none is
/// given), or remove its name.
pub async fn name<S>(client: &mut Client<S>, g: &Global, args: &[String]) -> i32
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let as_json = g.json.unwrap_or(false);
    let mut pane = "@current".to_string();
    let mut clear = false;
    let mut name: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--clear" => clear = true,
            "--pane" if i + 1 < args.len() => {
                i += 1;
                pane = args[i].clone();
            }
            a if a.starts_with("--") => {
                eprintln!("unknown flag `{a}`\n{NAME_USAGE}");
                return EXIT_USAGE;
            }
            a if name.is_none() => name = Some(bare_name(a).to_string()),
            a => {
                eprintln!("unexpected argument `{a}`\n{NAME_USAGE}");
                return EXIT_USAGE;
            }
        }
        i += 1;
    }
    if clear && name.is_some() {
        eprintln!("--clear takes no name\n{NAME_USAGE}");
        return EXIT_USAGE;
    }
    if let Err(e) = client.hello("cli").await {
        print_error(&e);
        return exit_code_for(&e);
    }
    let (method, params) = if clear {
        ("mention.clear", json!({"pane": pane}))
    } else {
        let mut p = json!({"pane": pane});
        if let Some(n) = &name {
            p["name"] = json!(n);
        }
        ("mention.set", p)
    };
    match client.call(method, params).await {
        Ok(r) => {
            if as_json {
                println!("{}", serde_json::to_string(&r).unwrap_or_default());
            } else if clear {
                println!("name removed");
            } else {
                println!("@{}", r["name"].as_str().unwrap_or(""));
            }
            EXIT_OK
        }
        Err(e) => {
            print_error(&e);
            exit_code_for(&e)
        }
    }
}

fn print_event(e: &Value) {
    println!("{}", serde_json::to_string(e).unwrap_or_default());
}

/// `vibeke events tail`: recent events, then with `--follow` a live subscription (resubscribing
/// after an overflow, so nothing is lost silently).
pub async fn events_tail<S>(client: &mut Client<S>, _g: &Global, args: &[String]) -> i32
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let p = match build_params(&[], args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}\n{EVENTS_TAIL_USAGE}");
            return EXIT_USAGE;
        }
    };
    let follow = p.get("follow").and_then(Value::as_bool).unwrap_or(false);
    let types: Vec<String> = match p.get("types") {
        Some(Value::String(s)) => s.split(',').map(str::to_string).collect(),
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .flat_map(|s| s.split(','))
            .map(str::to_string)
            .collect(),
        _ => vec![],
    };
    let lines = p.get("lines").and_then(Value::as_u64).unwrap_or(20) as usize;
    let fail = |e: CallError| {
        print_error(&e);
        exit_code_for(&e)
    };
    if let Err(e) = client.hello("cli").await {
        return fail(e);
    }
    let head = match client.call("server.status", json!({})).await {
        Ok(v) => v["event_seq"].as_i64().unwrap_or(0),
        Err(e) => return fail(e),
    };
    let mut last = match p.get("after_seq").and_then(Value::as_i64) {
        // Explicit start: replay everything after it.
        Some(after) => {
            let mut after = after;
            loop {
                let r = client
                    .call(
                        "events.read",
                        json!({"after": after, "types": types, "limit": 500}),
                    )
                    .await;
                let v = match r {
                    Ok(v) => v,
                    Err(e) => return fail(e),
                };
                let evs = v["events"].as_array().cloned().unwrap_or_default();
                for e in &evs {
                    print_event(e);
                    after = e["seq"].as_i64().unwrap_or(after);
                }
                if evs.len() < 500 {
                    break after;
                }
            }
        }
        // The last `lines` events (filters apply within a bounded window of recent events).
        None => {
            let window = if types.is_empty() {
                lines as i64
            } else {
                (lines as i64 * 50).max(2000)
            };
            let mut after = (head - window).max(0);
            let mut kept: std::collections::VecDeque<Value> = Default::default();
            loop {
                let v = match client
                    .call(
                        "events.read",
                        json!({"after": after, "types": types, "limit": 500}),
                    )
                    .await
                {
                    Ok(v) => v,
                    Err(e) => return fail(e),
                };
                let evs = v["events"].as_array().cloned().unwrap_or_default();
                for e in evs.iter() {
                    after = e["seq"].as_i64().unwrap_or(after);
                    kept.push_back(e.clone());
                    if kept.len() > lines {
                        kept.pop_front();
                    }
                }
                if evs.len() < 500 || after >= head {
                    break;
                }
            }
            for e in &kept {
                print_event(e);
            }
            head.max(after)
        }
    };
    if !follow {
        return EXIT_OK;
    }
    'subscribe: loop {
        if let Err(e) = client
            .call("events.subscribe", json!({"after": last, "types": types}))
            .await
        {
            return fail(e);
        }
        // Pushes that arrived with the subscribe response.
        let mut pending: Vec<Value> = std::mem::take(&mut client.notifications);
        loop {
            let msg = if pending.is_empty() {
                match client.recv().await {
                    Ok(m) => m,
                    Err(e) => {
                        eprintln!("{e:#}");
                        return crate::EXIT_API;
                    }
                }
            } else {
                pending.remove(0)
            };
            match msg.get("method").and_then(Value::as_str) {
                Some("events.event") => {
                    let e = &msg["params"]["event"];
                    let seq = e["seq"].as_i64().unwrap_or(0);
                    if seq == 0 && e["tier"] == "transient" {
                        // A transient notification (`assistant.delta`): not outbox history,
                        // so it never moves the cursor.
                        print_event(e);
                    } else if seq > last {
                        last = seq;
                        print_event(e);
                    }
                }
                Some("events.overflow") => {
                    if let Some(s) = msg["params"]["resume_from"]["seq"].as_i64() {
                        last = last.min(s).max(0);
                    }
                    continue 'subscribe;
                }
                _ => {}
            }
        }
    }
}

// ---- completion ---------------------------------------------------------------------------------

/// Top-level commands that are not API nouns, with their verbs.
const EXTRA: &[(&str, &[&str])] = &[
    ("attach", &[]),
    ("ssh", &[]),
    ("attach-file", &[]),
    ("notify", &[]),
    ("search", &[]),
    ("focus", &[]),
    ("import", &["herdr"]),
    (
        "plugin",
        &[
            "list", "install", "link", "unlink", "trust", "enable", "disable", "remove", "action",
            "logs", "pane",
        ],
    ),
    ("compat", &["herdr", "install-shim", "status"]),
    (
        "integration",
        &[
            "list",
            "status",
            "install",
            "uninstall",
            "doctor",
            "capabilities",
            "update",
        ],
    ),
    ("mcp", &[]),
    ("msg", &["reply", "done", "inbox", "list"]),
    ("name", &[]),
    ("doctor", &[]),
    ("forget", &[]),
    ("handoff", crate::handoff::VERBS),
    ("share", &["watch", "review", "list", "revoke"]),
    ("security", &["keychain"]),
    ("update", &[]),
    (
        "server",
        &["start", "stop", "status", "restart", "reload-config"],
    ),
    (
        "gateway",
        &[
            "run", "pair", "share", "devices", "invites", "revoke", "peer", "status",
        ],
    ),
    ("relay", &[]),
    ("api", &["schema", "methods", "call"]),
    (
        "config",
        &[
            "path",
            "validate",
            "default",
            "get",
            "set",
            "reload",
            "edit",
            "reset-keys",
        ],
    ),
    ("shell-integration", &["zsh", "bash", "fish"]),
    ("keys", &["check"]),
    (
        "machine",
        &[
            "list",
            "add",
            "remove",
            "status",
            "connect",
            "disconnect",
            "upgrade",
        ],
    ),
    ("events", &["tail"]),
    ("cloud", &["login", "send", "bring-back"]),
    (
        "debug",
        &["latency", "bandwidth", "api-schema", "idle", "ptyshot"],
    ),
    ("completion", &["bash", "zsh", "fish", "nu", "powershell"]),
    ("help", &[]),
    ("version", &[]),
];

pub const SHELLS: &[&str] = &["bash", "zsh", "fish", "nu", "powershell"];

/// noun -> verbs, from `COMMANDS` plus the non-API commands.
pub fn command_tree() -> BTreeMap<String, Vec<String>> {
    let mut t: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (noun, verb, ..) in COMMANDS {
        t.entry(noun.to_string())
            .or_default()
            .push(verb.to_string());
    }
    for (noun, verbs) in EXTRA {
        let e = t.entry(noun.to_string()).or_default();
        e.extend(verbs.iter().map(|v| v.to_string()));
    }
    for v in t.values_mut() {
        v.sort();
        v.dedup();
    }
    t
}

/// The completion script for `shell`, or `None` for an unknown shell.
pub fn completion_script(shell: &str) -> Option<String> {
    let tree = command_tree();
    let nouns: Vec<&str> = tree.keys().map(String::as_str).collect();
    let top = nouns.join(" ");
    let mut s = String::new();
    match shell {
        "bash" => {
            s.push_str("# vibeke completion for bash: eval \"$(vibeke completion bash)\"\n_vibeke() {\n  local cur=\"${COMP_WORDS[COMP_CWORD]}\"\n  if [ \"$COMP_CWORD\" -eq 1 ]; then\n");
            s.push_str(&format!(
                "    COMPREPLY=($(compgen -W \"{top}\" -- \"$cur\"))\n    return\n  fi\n  [ \"$COMP_CWORD\" -eq 2 ] || return\n  case \"${{COMP_WORDS[1]}}\" in\n"
            ));
            for (n, v) in tree.iter().filter(|(_, v)| !v.is_empty()) {
                s.push_str(&format!(
                    "    {n}) COMPREPLY=($(compgen -W \"{}\" -- \"$cur\")) ;;\n",
                    v.join(" ")
                ));
            }
            s.push_str("  esac\n}\ncomplete -o default -F _vibeke vibeke\n");
        }
        "zsh" => {
            s.push_str("#compdef vibeke\n# vibeke completion for zsh: source <(vibeke completion zsh), or save as _vibeke in $fpath\n_vibeke() {\n");
            s.push_str(&format!(
                "  if (( CURRENT == 2 )); then\n    compadd -- {top}\n    return\n  fi\n  (( CURRENT == 3 )) || {{ _files; return }}\n  case $words[2] in\n"
            ));
            for (n, v) in tree.iter().filter(|(_, v)| !v.is_empty()) {
                s.push_str(&format!("    {n}) compadd -- {} ;;\n", v.join(" ")));
            }
            s.push_str("    *) _files ;;\n  esac\n}\nif [[ \"${funcstack[1]}\" == \"_vibeke\" ]]; then\n  _vibeke \"$@\"\nelse\n  compdef _vibeke vibeke\nfi\n");
        }
        "fish" => {
            s.push_str("# vibeke completion for fish: vibeke completion fish | source\n");
            s.push_str(&format!(
                "complete -c vibeke -f -n '__fish_use_subcommand' -a '{top}'\n"
            ));
            for (n, v) in tree.iter().filter(|(_, v)| !v.is_empty()) {
                s.push_str(&format!(
                    "complete -c vibeke -f -n '__fish_seen_subcommand_from {n}; and test (count (commandline -opc)) -eq 2' -a '{}'\n",
                    v.join(" ")
                ));
            }
        }
        "nu" => {
            s.push_str("# vibeke completion for nushell: vibeke completion nu | save -f ~/.config/nushell/vibeke.nu; then `use ~/.config/nushell/vibeke.nu *`\n");
            s.push_str("def \"nu-complete vibeke nouns\" [] {\n  [");
            s.push_str(
                &nouns
                    .iter()
                    .map(|n| format!("\"{n}\""))
                    .collect::<Vec<_>>()
                    .join(" "),
            );
            s.push_str("]\n}\n\ndef \"nu-complete vibeke verbs\" [context: string] {\n  let noun = ($context | split row ' ' | where {|w| $w != ''} | get 1? | default '')\n  match $noun {\n");
            for (n, v) in tree.iter().filter(|(_, v)| !v.is_empty()) {
                s.push_str(&format!(
                    "    \"{n}\" => [{}]\n",
                    v.iter()
                        .map(|x| format!("\"{x}\""))
                        .collect::<Vec<_>>()
                        .join(" ")
                ));
            }
            s.push_str("    _ => []\n  }\n}\n\nexport extern \"vibeke\" [\n  noun?: string@\"nu-complete vibeke nouns\"\n  verb?: string@\"nu-complete vibeke verbs\"\n  ...rest: string\n]\n");
        }
        "powershell" => {
            s.push_str("# vibeke completion for PowerShell: vibeke completion powershell | Out-String | Invoke-Expression\n");
            s.push_str("Register-ArgumentCompleter -Native -CommandName vibeke -ScriptBlock {\n  param($wordToComplete, $commandAst, $cursorPosition)\n  $verbs = @{\n");
            for (n, v) in &tree {
                s.push_str(&format!(
                    "    '{n}' = @({})\n",
                    v.iter()
                        .map(|x| format!("'{x}'"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            s.push_str("  }\n  $words = @($commandAst.CommandElements | ForEach-Object { $_.ToString() })\n  $index = $words.Count\n  if ($wordToComplete -ne '') { $index -= 1 }\n  if ($index -le 1) { $candidates = $verbs.Keys }\n  elseif ($index -eq 2) { $candidates = $verbs[$words[1]] }\n  else { $candidates = @() }\n  $candidates | Where-Object { $_ -like \"$wordToComplete*\" } | Sort-Object | ForEach-Object {\n    [System.Management.Automation.CompletionResult]::new($_, $_, 'ParameterValue', $_)\n  }\n}\n");
        }
        _ => return None,
    }
    Some(s)
}

/// `vibeke completion <shell>`.
pub fn completion(args: &[String]) -> i32 {
    let shell = args.first().map(String::as_str).unwrap_or("");
    match completion_script(shell) {
        Some(s) => {
            print!("{s}");
            EXIT_OK
        }
        None => {
            eprintln!("vibeke completion <{}>", SHELLS.join("|"));
            EXIT_USAGE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_cover_nouns_and_verbs() {
        for sh in SHELLS {
            let s = completion_script(sh).unwrap();
            assert!(s.contains("session"), "{sh}");
            assert!(s.contains("screenshot"), "{sh}");
            assert!(s.contains("powershell"), "{sh}");
        }
        assert!(completion_script("tcsh").is_none());
        let t = command_tree();
        assert!(t["events"].contains(&"tail".to_string()));
        assert!(t["task"].contains(&"park".to_string()));
    }
}
