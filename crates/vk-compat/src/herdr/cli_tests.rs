use super::*;

fn args(s: &str) -> Vec<String> {
    s.split_whitespace().map(String::from).collect()
}

fn env() -> Env {
    Env {
        pane_id: Some("w1:p7".into()),
        cwd: Some(PathBuf::from("/work")),
        home: Some(PathBuf::from("/home/me")),
    }
}

fn p(s: &str) -> Parsed {
    parse_with(&args(s), &env())
}

fn pv(v: &[&str]) -> Parsed {
    parse_with(&v.iter().map(|x| x.to_string()).collect::<Vec<_>>(), &env())
}

/// `(id, method, params)` of a socket call.
fn call(s: &str) -> (String, String, Value) {
    match p(s) {
        Parsed::Call {
            id, method, params, ..
        } => (id, method, params),
        other => panic!("{s}: {other:?}"),
    }
}

fn usage_of(s: &str) -> (String, i32) {
    match p(s) {
        Parsed::Usage { text, code } => (text, code),
        other => panic!("{s}: {other:?}"),
    }
}

#[test]
fn request_ids_are_herdrs() {
    for (cmd, id) in [
        ("pane list", "cli:pane:list"),
        ("pane get w1:p1", "cli:pane:get"),
        ("pane current", "cli:pane:current"),
        ("pane layout", "cli:pane:layout"),
        ("pane process-info", "cli:pane:process_info"),
        ("pane read w1:p1", "cli:pane:read"),
        ("pane split --direction right", "cli:pane:split"),
        ("pane send-text w1:p1 hi", "cli:request"),
        ("pane run w1:p1 ls", "cli:request"),
        ("workspace list", "cli:workspace:list"),
        ("tab get t1", "cli:tab:get"),
        ("agent focus a", "cli:agent:focus"),
        ("agent send-keys a Enter", "cli:agent:send-keys"),
        ("worktree remove --workspace w1", "cli:worktree:remove"),
        ("notification show Hi", "cli:notification:show"),
        ("plugin action list", "cli:plugin"),
        ("plugin pane open --plugin a --entrypoint b", "cli:plugin"),
        ("server reload-config", "cli:server:reload-config"),
        ("api snapshot", "cli:api:snapshot"),
        ("terminal title set x", "cli:terminal:title:set"),
    ] {
        assert_eq!(call(cmd).0, id, "{cmd}");
    }
}

#[test]
fn output_follows_herdrs_printer() {
    let out = |s: &str| match p(s) {
        Parsed::Call { output, .. } => output,
        other => panic!("{s}: {other:?}"),
    };
    assert_eq!(out("pane get w1:p1"), Output::Response);
    assert_eq!(out("pane send-keys w1:p1 Enter"), Output::Quiet);
    assert_eq!(
        out("pane report-metadata w1:p1 --source s --title t"),
        Output::Quiet
    );
    assert_eq!(out("pane read w1:p1"), Output::ReadText);
    assert_eq!(out("agent read a"), Output::ReadText);
    assert_eq!(
        out("agent explain a --json"),
        Output::Explain {
            json: true,
            verbose: false
        }
    );
}

#[test]
fn plugin_pane_open_maps_every_flag() {
    let v = [
        "plugin",
        "pane",
        "open",
        "--plugin",
        "acme.x",
        "--entrypoint",
        "root-picker",
        "--placement",
        "popup",
        "--width",
        "80%",
        "--height",
        "20",
        "--workspace",
        "w1",
        "--target-pane",
        "w1:p2",
        "--direction",
        "down",
        "--cwd",
        "/tmp",
        "--env",
        "A=1",
        "--env",
        "B=x=y",
        "--no-focus",
    ];
    let Parsed::Call { method, params, .. } = pv(&v) else {
        panic!()
    };
    assert_eq!(method, "plugin.pane.open");
    assert_eq!(
        params,
        json!({
            "plugin_id": "acme.x", "entrypoint": "root-picker", "placement": "popup",
            "width": "80%", "height": 20, "workspace_id": "w1", "target_pane_id": "w1:p2",
            "direction": "down", "cwd": "/tmp", "focus": false,
            "env": {"A": "1", "B": "x=y"}
        })
    );
    // Focus defaults to true; `fullscreen` is an alias of `zoomed`.
    assert_eq!(
        call("plugin pane open --plugin a --entrypoint b --placement fullscreen").2,
        json!({"plugin_id": "a", "entrypoint": "b", "placement": "zoomed", "focus": true})
    );
    assert_eq!(
        usage_of("plugin pane open --entrypoint b"),
        ("missing required --plugin".into(), 2)
    );
    assert_eq!(
        usage_of("plugin pane open --plugin a --entrypoint b --placement corner").0,
        "invalid pane placement: corner"
    );
    assert_eq!(
        usage_of("plugin pane open --plugin a --entrypoint b --env NOEQUALS").0,
        "env must use KEY=VALUE"
    );
    assert_eq!(
        usage_of("plugin pane open --plugin a --entrypoint b --width 0%").0,
        "--width percentage must be between 1% and 100%"
    );
}

#[test]
fn plugin_pane_focus_and_close_take_a_pane_id() {
    assert_eq!(
        call("plugin pane focus w1:p3"),
        (
            "cli:plugin".into(),
            "plugin.pane.focus".into(),
            json!({"pane_id": "w1:p3"})
        )
    );
    assert_eq!(
        call("plugin pane close w1:p3").2,
        json!({"pane_id": "w1:p3"})
    );
    assert_eq!(
        usage_of("plugin pane close a b"),
        ("usage: herdr plugin pane close <pane_id>".into(), 2)
    );
}

#[test]
fn pane_split_and_list_use_herdrs_parameter_names() {
    assert_eq!(
        call("pane list --workspace w1").2,
        json!({"workspace_id": "w1"})
    );
    // The caller's pane is the default target; a positional or --pane overrides it.
    assert_eq!(
        call("pane split --direction right").2,
        json!({"target_pane_id": "w1:p7", "direction": "right", "focus": false, "right_click": "herdr"})
    );
    assert_eq!(
        call("pane split w1:p1 --direction down --ratio 0.3 --cwd /x --env K=V --focus --right-click=pane").2,
        json!({
            "target_pane_id": "w1:p1", "direction": "down", "ratio": 0.3, "cwd": "/x",
            "focus": true, "right_click": "pane", "env": {"K": "V"}
        })
    );
    assert_eq!(
        call("pane split --pane w1:p2 --right-click pane --direction right").2["target_pane_id"],
        "w1:p2"
    );
    assert_eq!(
        usage_of("pane split w1:p1 --direction left").0,
        "invalid split direction: left"
    );
    assert_eq!(
        usage_of("pane split w1:p1 --ratio x --direction right").0,
        "invalid ratio: x"
    );
    assert!(
        usage_of("pane split w1:p1")
            .0
            .starts_with("usage: herdr pane split")
    );
    let no_pane = Env::default();
    assert_eq!(
        parse_with(&args("pane split --current --direction right"), &no_pane),
        Parsed::Usage {
            text: "--current requires HERDR_PANE_ID".into(),
            code: 2
        }
    );
}

#[test]
fn current_resolves_to_the_callers_pane() {
    assert_eq!(call("pane layout --current").2, json!({"pane_id": "w1:p7"}));
    assert_eq!(call("pane layout").2, json!({}));
    assert_eq!(
        call("pane layout --pane w1:p2"),
        (
            "cli:pane:layout".into(),
            "pane.layout".into(),
            json!({"pane_id": "w1:p2"})
        )
    );
    assert_eq!(
        call("pane process-info --current"),
        (
            "cli:pane:process_info".into(),
            "pane.process_info".into(),
            json!({"pane_id": "w1:p7"})
        )
    );
    assert_eq!(call("pane current").2, json!({"caller_pane_id": "w1:p7"}));
    assert_eq!(
        call("pane input --current --right-click pane").2,
        json!({"pane_id": "w1:p7", "right_click": "pane"})
    );
    assert_eq!(
        usage_of("pane layout --bogus"),
        ("unknown option: --bogus".into(), 2)
    );
}

#[test]
fn neighbor_focus_resize_zoom_swap_move() {
    assert_eq!(
        call("pane neighbor --pane w1:p1 --direction left"),
        (
            "cli:pane:neighbor".into(),
            "pane.neighbor".into(),
            json!({"pane_id": "w1:p1", "direction": "left"})
        )
    );
    assert!(
        usage_of("pane neighbor --pane w1:p1")
            .0
            .starts_with("usage: herdr pane neighbor")
    );
    assert_eq!(
        call("pane focus --direction up"),
        (
            "cli:pane:focus".into(),
            "pane.focus_direction".into(),
            json!({"direction": "up"})
        )
    );
    assert!(
        usage_of("pane focus w1:p1")
            .0
            .starts_with("usage: herdr pane focus")
    );
    assert_eq!(
        call("pane resize --direction right --amount 0.1").2,
        json!({"direction": "right", "amount": 0.1})
    );
    assert_eq!(
        call("pane zoom w1:p1 --on").2,
        json!({"pane_id": "w1:p1", "mode": "on"})
    );
    assert_eq!(call("pane zoom").2, json!({"mode": "toggle"}));
    assert_eq!(
        usage_of("pane zoom --on --off").0,
        "provide only one of --toggle, --on, or --off"
    );
    assert_eq!(
        call("pane swap --source-pane w1:p1 --target-pane w1:p2").2,
        json!({"source_pane_id": "w1:p1", "target_pane_id": "w1:p2"})
    );
    assert!(
        usage_of("pane swap w1:p1 w1:p2")
            .0
            .starts_with("unknown option")
    );
    assert_eq!(
        call("pane move w1:p2 --tab t2 --split down").2,
        json!({"pane_id": "w1:p2", "destination": {"type": "tab", "tab_id": "t2", "split": "down"}, "focus": true})
    );
    assert_eq!(
        call("pane move w1:p2 --new-workspace --label x --no-focus").2,
        json!({"pane_id": "w1:p2", "destination": {"type": "new_workspace", "label": "x"}, "focus": false})
    );
    assert!(
        usage_of("pane move w1:p2 --tab t2")
            .0
            .starts_with("usage: herdr pane move")
    );
}

#[test]
fn pane_run_sends_input_with_enter() {
    assert_eq!(
        call("pane run w1:p1 echo hi"),
        (
            "cli:request".into(),
            "pane.send_input".into(),
            json!({"pane_id": "w1:p1", "text": "echo hi", "keys": ["Enter"]})
        )
    );
    assert_eq!(
        usage_of("pane run w1:p1"),
        ("usage: herdr pane run <pane_id> <command>".into(), 2)
    );
}

#[test]
fn text_commands_keep_strings() {
    assert_eq!(
        call("pane send-text w1:p2 42").2,
        json!({"pane_id": "w1:p2", "text": "42"})
    );
    assert_eq!(
        call("pane send-keys w1:p2 Enter C-c").2,
        json!({"pane_id": "w1:p2", "keys": ["Enter", "C-c"]})
    );
    assert_eq!(
        call("pane rename w1:p2 my logs").2,
        json!({"pane_id": "w1:p2", "label": "my logs"})
    );
    assert_eq!(
        call("pane rename w1:p2 --clear").2,
        json!({"pane_id": "w1:p2"})
    );
    assert_eq!(
        call("pane read w1:p2 --source recent-unwrapped --lines 40 --ansi").2,
        json!({"pane_id": "w1:p2", "source": "recent_unwrapped", "lines": 40, "format": "ansi", "strip_ansi": true})
    );
    assert_eq!(
        call("pane read w1:p2 --raw").2,
        json!({"pane_id": "w1:p2", "source": "recent", "format": "ansi", "strip_ansi": false})
    );
    assert_eq!(
        call("pane wait-output w1:p1 --regex ^ok$ --timeout=500").2,
        json!({"pane_id": "w1:p1", "source": "recent", "match": {"type": "regex", "value": "^ok$"}, "timeout_ms": 500, "strip_ansi": true})
    );
    assert_eq!(
        usage_of("pane wait-output w1:p1").0,
        "missing required --match or --regex"
    );
}

#[test]
fn report_commands() {
    assert_eq!(
        call(
            "pane report-agent w1:p1 --source s --agent a --state working --seq 3 -- tool --resume"
        )
        .2,
        json!({
            "pane_id": "w1:p1", "source": "s", "agent": "a", "state": "working", "seq": 3,
            "resume_argv": ["tool", "--resume"]
        })
    );
    assert_eq!(
        p("pane report-agent w1:p1 --source s --agent a --state sleepy"),
        Parsed::Failed(
            "invalid pane agent state: sleepy (expected idle, working, blocked, or unknown)".into()
        )
    );
    assert_eq!(
        usage_of("pane report-agent w1:p1 --agent a --state idle").0,
        "missing required --source"
    );
    assert_eq!(
        call("pane report-metadata w1:p1 --source s --token ci=green --clear-token old --state-label Done=Fin").2,
        json!({
            "pane_id": "w1:p1", "source": "s", "state_labels": {"done": "Fin"},
            "tokens": {"ci": "green", "old": null},
            "clear_title": false, "clear_display_agent": false, "clear_state_labels": false
        })
    );
    assert_eq!(
        usage_of("pane report-metadata w1:p1 --source s").0,
        "missing metadata field to set or clear"
    );
    assert_eq!(
        usage_of("pane report-metadata w1:p1 --ci green").0,
        "unknown option: --ci"
    );
    assert_eq!(
        call("workspace report-metadata w1 --source s --token branch=main").2,
        json!({"workspace_id": "w1", "source": "s", "tokens": {"branch": "main"}})
    );
    assert_eq!(
        call("pane release-agent w1:p1 --source s --agent a").1,
        "pane.release_agent"
    );
}

#[test]
fn workspace_tab_worktree() {
    assert_eq!(
        call("workspace create --cwd /tmp --focus --label x --env A=1").2,
        json!({"cwd": "/tmp", "focus": true, "label": "x", "env": {"A": "1"}})
    );
    assert_eq!(
        call("workspace close w1 --group").2,
        json!({"workspace_id": "w1", "close_group": true})
    );
    assert_eq!(call("workspace close w1").2, json!({"workspace_id": "w1"}));
    assert_eq!(
        call("tab create --workspace w1 --label two").2,
        json!({"workspace_id": "w1", "focus": false, "label": "two"})
    );
    assert_eq!(
        call("tab get t1"),
        (
            "cli:tab:get".into(),
            "tab.get".into(),
            json!({"tab_id": "t1"})
        )
    );
    assert_eq!(
        usage_of("tab create --workspace-id w1"),
        ("unknown option: --workspace-id".into(), 2)
    );
    assert_eq!(
        call("worktree open --path rel --label x").2,
        json!({"path": "/work/rel", "label": "x", "focus": false})
    );
    assert_eq!(
        call("worktree open --branch b --cwd ~/repo --json").2,
        json!({"cwd": "/home/me/repo", "branch": "b", "focus": false})
    );
    // A positional path is not Herdr's grammar.
    assert_eq!(
        usage_of("worktree open /tmp/x"),
        ("unknown option: /tmp/x".into(), 2)
    );
    assert!(
        usage_of("worktree open")
            .0
            .starts_with("usage: herdr worktree open")
    );
    assert_eq!(
        call("worktree remove --workspace w1 --force").2,
        json!({"workspace_id": "w1", "force": true})
    );
    assert!(
        usage_of("worktree remove")
            .0
            .starts_with("usage: herdr worktree remove")
    );
    assert_eq!(
        call("worktree create --cwd /r --branch b --base main --trust-repository").2,
        json!({"cwd": "/r", "branch": "b", "base": "main", "focus": false, "trust_repository": true})
    );
}

#[test]
fn agent_commands() {
    assert_eq!(call("agent focus a").2, json!({"target": "a"}));
    assert_eq!(
        call("agent prompt a hello --wait --until idle --timeout 5").2,
        json!({"target": "a", "text": "hello", "wait": {"until": ["idle"], "timeout_ms": 5}})
    );
    assert_eq!(
        usage_of("agent prompt a hello --until idle").0,
        "--until requires --wait"
    );
    assert_eq!(
        call("agent wait a --until done --until blocked").2,
        json!({"target": "a", "until": ["done", "blocked"]})
    );
    assert!(
        usage_of("agent wait a --status x")
            .0
            .starts_with("unknown option")
    );
    assert_eq!(call("agent rename a --clear").2, json!({"target": "a"}));
    assert_eq!(
        call("agent start helper --kind k --pane w1:p1 --timeout 9 -- --flag").2,
        json!({"name": "helper", "kind": "k", "pane_id": "w1:p1", "args": ["--flag"], "timeout_ms": 9})
    );
    assert_eq!(
        usage_of("agent start helper --pane w1:p1").0,
        "missing required --kind"
    );
    assert_eq!(
        call("agent read a --format ansi").2,
        json!({"target": "a", "source": "recent", "format": "ansi", "strip_ansi": false})
    );
    assert!(matches!(p("agent attach a"), Parsed::Refused { .. }));
    assert!(usage_of("agent").0.contains("kinds: "));
}

#[test]
fn notification_and_plugin_actions() {
    assert_eq!(
        call("notification show Hi --body there --sound done").2,
        json!({"title": "Hi", "body": "there", "sound": "done"})
    );
    assert_eq!(
        usage_of("notification show Hi there"),
        ("unknown option: there".into(), 2)
    );
    assert_eq!(
        call("plugin action invoke acme.x.open"),
        (
            "cli:plugin".into(),
            "plugin.action.invoke".into(),
            json!({"action_id": "acme.x.open", "context": {"invocation_source": "cli"}})
        )
    );
    assert_eq!(
        call("plugin action invoke open --plugin acme.x").2,
        json!({"action_id": "open", "plugin_id": "acme.x", "context": {"invocation_source": "cli"}})
    );
    assert_eq!(
        call("plugin action list --plugin acme.x").2,
        json!({"plugin_id": "acme.x"})
    );
    assert_eq!(
        call("plugin log list --plugin acme.x --limit 5").2,
        json!({"plugin_id": "acme.x", "limit": 5})
    );
    assert_eq!(call("plugin logs").2, json!({}));
}

#[test]
fn local_commands() {
    assert_eq!(
        p("plugin install o/r --ref v1 -y"),
        Parsed::Local(Local::PluginInstall {
            source: "o/r".into(),
            git_ref: Some("v1".into()),
            yes: true
        })
    );
    assert_eq!(
        p("plugin link ./x --disabled"),
        Parsed::Local(Local::PluginLink {
            path: "./x".into(),
            yes: false,
            enabled: false
        })
    );
    assert_eq!(p("plugin list --json"), Parsed::Local(Local::PluginList));
    assert_eq!(
        p("plugin config-dir acme.x"),
        Parsed::Local(Local::PluginConfigDir {
            id: "acme.x".into()
        })
    );
    assert_eq!(
        p("plugin update acme.x --yes"),
        Parsed::Local(Local::PluginUpdate {
            id: "acme.x".into(),
            yes: true
        })
    );
    assert_eq!(
        usage_of("plugin unlink"),
        ("usage: herdr plugin unlink <plugin_id>".into(), 2)
    );
    assert_eq!(p("config check"), Parsed::ConfigCheck);
    assert_eq!(usage_of("config check /tmp/c.toml").1, 2);
    assert_eq!(p("session list --json"), Parsed::SessionList { json: true });
    assert_eq!(p("session list"), Parsed::SessionList { json: false });
    assert_eq!(
        p("status server"),
        Parsed::Status {
            scope: StatusScope::Server,
            json: false
        }
    );
    assert_eq!(
        p("status --json"),
        Parsed::Status {
            scope: StatusScope::Full,
            json: true
        }
    );
    assert_eq!(
        usage_of("status server --x"),
        ("usage: herdr status server [--json]".into(), 2)
    );
    assert_eq!(p("api schema --json"), Parsed::ApiSchema(ApiSchema::Json));
    assert!(api_schema_summary().starts_with("Herdr API schema\nprotocol: 22\n"));
}

#[test]
fn help_and_global_flags() {
    let Parsed::Help(page) = p("pane split --help") else {
        panic!()
    };
    assert!(page.starts_with("Split a pane\n\nUsage: herdr pane split [OPTIONS] [PANE_ID]"));
    assert!(matches!(p("plugin pane open -h"), Parsed::Help(_)));
    // `--help` after other arguments goes to the command's own parser, like Herdr.
    assert_eq!(
        usage_of("pane get w1 --help").0,
        "usage: herdr pane get <pane_id>"
    );
    let Parsed::Help(cfg) = p("--default-config") else {
        panic!()
    };
    assert!(cfg.starts_with("# herdr configuration"));
    assert!(cfg.contains("[keys]"));
    assert_eq!(p("--version"), Parsed::Version);
    assert!(matches!(p(""), Parsed::Help(_)));
    assert_eq!(
        usage_of("bogus"),
        (
            "unknown command: bogus\nrun 'herdr --help' for usage".into(),
            2
        )
    );
    assert_eq!(
        usage_of("--bogus"),
        (
            "unknown option: --bogus\nrun 'herdr --help' for usage".into(),
            2
        )
    );
    // Group help: on stderr, exit 2 without a subcommand, 0 for `help`.
    assert_eq!(usage_of("pane").1, 2);
    assert_eq!(usage_of("pane help").1, 0);
    assert!(usage_of("pane fly").0.starts_with("herdr pane commands:"));
    assert!(matches!(p("update"), Parsed::Refused { .. }));
    assert!(matches!(p("machine list"), Parsed::Refused { .. }));
    assert!(matches!(p("terminal attach t1"), Parsed::Refused { .. }));
    assert!(matches!(
        p("integration install claude"),
        Parsed::Refused { .. }
    ));
    // Commands Herdr does not have are unknown, as in Herdr.
    for gone in ["ping", "events subscribe", "layout export", "popup close"] {
        assert_eq!(usage_of(gone).1, 2, "{gone}");
    }
    assert_eq!(usage_of("session snapshot").1, 2);
}

#[test]
fn every_captured_help_page_is_served() {
    let index = include_str!("../../../../tests/compat/herdr/0.9.3/cli-help/index.txt");
    for line in index.lines() {
        let cmd = line.split('\t').next().unwrap_or_default();
        let path: Vec<&str> = cmd
            .split_whitespace()
            .skip(1)
            .take_while(|w| *w != "--help")
            .collect();
        if path.is_empty() {
            continue;
        }
        let mut a: Vec<String> = path.iter().map(|w| w.to_string()).collect();
        a.push("--help".into());
        assert!(matches!(parse_with(&a, &env()), Parsed::Help(_)), "{cmd}");
    }
}

#[test]
fn completion_and_session_flag() {
    for sh in ["bash", "zsh", "fish"] {
        let Parsed::Help(text) = p(&format!("completion {sh}")) else {
            panic!("{sh}");
        };
        assert!(
            text.contains("plugin") && text.contains("process-info"),
            "{sh}"
        );
        assert!(!text.contains("\\n"), "{sh}: literal escapes");
    }
    assert_eq!(usage_of("completion").1, 2);
    assert!(
        usage_of("completion tcsh")
            .0
            .starts_with("unknown shell: tcsh")
    );
    assert_eq!(
        take_session(&args("--session work pane list")).unwrap(),
        (Some("work".into()), args("pane list"))
    );
    assert_eq!(
        take_session(&args("pane list --session=w2")).unwrap(),
        (Some("w2".into()), args("pane list"))
    );
    assert_eq!(
        take_session(&args("pane send-text p -- --session x")).unwrap(),
        (None, args("pane send-text p -- --session x")),
        "after -- it is text"
    );
    assert!(take_session(&args("--session")).is_err());
    assert!(take_session(&args("--session ../x pane list")).is_err());
}

#[test]
fn shim_methods_are_what_the_grammar_sends() {
    let methods = shim_methods();
    for cmd in [
        "pane layout",
        "pane neighbor --direction up",
        "pane edges",
        "pane input w1:p1 --right-click pane",
        "agent send-keys a x",
        "worktree remove --workspace w1",
        "terminal title clear",
    ] {
        let m = call(cmd).1;
        assert!(methods.contains(&m.as_str()), "{cmd}: {m}");
    }
    assert!(!methods.contains(&"pane.run"));
}
