//! CLI verbs that are more than one API call (07 §5.3): `vibeke events tail [--follow]` and
//! `vibeke completion <shell>`.

use crate::client::{CallError, Client};
use crate::{COMMANDS, EXIT_OK, EXIT_USAGE, Global, build_params, exit_code_for, print_error};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub const EVENTS_TAIL_USAGE: &str = "vibeke events tail [--types 'agent.*,pane.created'] [--after-seq N | --lines N] [--follow]\n  one JSON event per line; without --after-seq the last N (default 20) events are shown first";

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
    ("doctor", &[]),
    ("forget", &[]),
    ("handoff", crate::handoff::VERBS),
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
