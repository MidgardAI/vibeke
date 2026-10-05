use std::path::Path;
use std::time::Duration;

use vk_config::*;

fn parse(src: &str) -> Result<(Config, Vec<Warning>), ConfigError> {
    Config::parse(src, Path::new("test.toml"))
}

#[test]
fn template_uncommented_equals_default_without_warnings() {
    let (cfg, warnings) = parse(default_config_toml_uncommented()).unwrap();
    assert_eq!(warnings, vec![]);
    assert_eq!(cfg, Config::default());
}

#[test]
fn commented_template_is_a_no_op() {
    let text = default_config_toml();
    assert!(
        text.lines()
            .all(|l| l.trim().is_empty() || l.starts_with('#'))
    );
    let (cfg, warnings) = parse(&text).unwrap();
    assert_eq!(warnings, vec![]);
    assert_eq!(cfg, Config::default());
    // Every action in the default keymap is documented in the file.
    for (action, _) in DEFAULT_KEYMAP {
        assert!(text.contains(&format!("# {action} ")), "{action}");
    }
}

#[test]
fn spec_defaults_spot_check() {
    let c = Config::default();
    assert_eq!(c.keys.prefix, "ctrl+b");
    assert_eq!(c.keys.prefix_timeout_ms, 1500);
    assert_eq!(c.keys.bindings["switch_tab"], "prefix+1..9");
    assert_eq!(c.keys.bindings["split_horizontal"], "prefix+minus");
    assert_eq!(c.terminal.archive_max_per_pane, ByteSize::mib(200));
    assert_eq!(c.paste.inbox_retention, Dur::secs(14 * 86400));
    assert_eq!(c.tasks.port_pool.to_string(), "20000-29999");
    assert_eq!(c.tasks.cleanup.stale_after, Dur::secs(14 * 86400));
    assert_eq!(c.ui.sidebar.width, 28);
    assert_eq!(c.ui.status_bar.right, vec!["agents_summary", "clock"]);
    assert_eq!(c.agents.harness["codex"].shim, Some(true));
    assert!(c.config.watch);
}

#[test]
fn full_example_parses() {
    let src = r##"
onboarding = true
[theme]
name = "nord"
[theme.custom]
accent = "#f5c2e7"
[terminal]
archive_max_per_pane = "1GiB"
[terminal.env]
EDITOR = "nvim"
[keys]
prefix = "ctrl+a"
fullscreen = "prefix+alt+w"
[keys.copy_mode]
mode = "emacs"
y = "copy"
[[keys.command]]
key = "prefix+alt+g"
type = "popup"
command = "lazygit"
width = "80%"
[[ui.sidebar.token]]
match = { harness = "codex" }
label = "cx"
[[ui.sidebar.token]]
match = { regex = "^tmp-" }
hide = true
[[policy.rule]]
match = { tool = "Bash", command_regex = '^(pnpm|npm) (test|run lint)( |$)' }
effect = "allow"
[tasks]
port_pool = "30000-30999"
[tasks.cleanup]
stale_after = "36h"
[[remote.machine]]
label = "devbox"
address = "demo@devbox.tailnet"
bootstrap = "remote-download"
[agents.harness.pi]
enabled = false
[preview]
mode = "proxy"
"##;
    let (c, w) = parse(src).unwrap();
    assert_eq!(w, vec![]);
    assert!(c.onboarding);
    assert_eq!(c.theme.name, "nord");
    assert_eq!(c.theme.custom["accent"], "#f5c2e7");
    assert_eq!(c.terminal.archive_max_per_pane, ByteSize(1 << 30));
    assert_eq!(c.terminal.env["EDITOR"], "nvim");
    assert_eq!(c.keys.prefix, "ctrl+a");
    // alias applied
    assert_eq!(c.keys.bindings["zoom"], "prefix+alt+w");
    // defaults still present
    assert_eq!(c.keys.bindings["help"], "prefix+?");
    assert_eq!(c.keys.copy_mode.mode, CopyModeKind::Emacs);
    assert_eq!(c.keys.copy_mode.overrides["y"], "copy");
    assert_eq!(c.keys.command[0].kind, CommandType::Popup);
    assert_eq!(c.ui.sidebar.token.len(), 2);
    assert!(c.ui.sidebar.token[1].hide);
    assert_eq!(c.policy.rule[0].effect, PolicyEffect::Allow);
    assert_eq!(c.tasks.port_pool.start, 30000);
    assert_eq!(c.tasks.cleanup.stale_after, Dur::secs(36 * 3600));
    assert_eq!(c.remote.machine[0].bootstrap, Bootstrap::RemoteDownload);
    assert!(c.remote.machine[0].auto_connect);
    // partial harness table keeps defaults for the keys it omits
    assert!(!c.agents.harness["pi"].enabled);
    assert_eq!(
        c.agents.harness["pi"].integration.as_deref(),
        Some("extension")
    );
    assert!(c.agents.harness.contains_key("claude"));
    assert_eq!(c.extra["preview"]["mode"].as_str(), Some("proxy"));
}

#[test]
fn unknown_keys_warn_with_dotted_path_and_line() {
    let src = "[ui]\nbogus = 1\n[ui.sidebar]\nwidth = 30\nnope = true\n[wat]\nx = 1\n[[policy.rule]]\nmatch = { tool = \"Bash\", extra = 1 }\neffect = \"deny\"\nzzz = 1\n[keys]\nnot_an_action = \"prefix+q\"\n";
    let (c, w) = parse(src).unwrap();
    assert_eq!(c.ui.sidebar.width, 30);
    let keys: Vec<&str> = w.iter().map(|w| w.key.as_str()).collect();
    for k in [
        "ui.bogus",
        "ui.sidebar.nope",
        "wat",
        "policy.rule[0].zzz",
        "policy.rule[0].match.extra",
        "keys.not_an_action",
    ] {
        assert!(keys.contains(&k), "missing warning for {k}: {keys:?}");
    }
    let w_bogus = w.iter().find(|w| w.key == "ui.bogus").unwrap();
    assert_eq!(w_bogus.pos.unwrap().line, 2);
    let w_nope = w.iter().find(|w| w.key == "ui.sidebar.nope").unwrap();
    assert_eq!(w_nope.pos.unwrap().line, 5);
}

#[test]
fn syntax_error_has_line_and_col() {
    let err = parse("onboarding = false\n[ui\nx = 1\n").unwrap_err();
    let d = err.first_diagnostic().unwrap();
    assert_eq!(d.file, Path::new("test.toml"));
    assert!(d.line >= 2, "{d}");
    assert!(d.col >= 1);
    assert!(err.to_string().starts_with("test.toml:"));
}

#[test]
fn type_error_has_location() {
    let err = parse("[ui]\nmax_fps = 60\n[ui.sidebar]\nwidth = \"wide\"\n").unwrap_err();
    let d = err.first_diagnostic().unwrap();
    assert_eq!(d.line, 4);
    assert!(matches!(err, ConfigError::Parse(_)));
}

#[test]
fn bad_enum_and_units_are_errors() {
    let err = parse("[ui.tabs]\nposition = \"middle\"\n").unwrap_err();
    let d = err.first_diagnostic().unwrap();
    assert_eq!(d.line, 2);
    assert!(d.message.contains("middle"), "{}", d.message);

    let err = parse("[terminal]\narchive_max_per_pane = \"lots\"\n").unwrap_err();
    assert_eq!(err.first_diagnostic().unwrap().line, 2);
    let err = parse("[paste]\ninbox_retention = \"14\"\n").unwrap_err();
    assert!(err.to_string().contains("unit"), "{err}");
    let err = parse("[tasks]\nport_pool = \"9-1\"\n").unwrap_err();
    assert!(err.to_string().contains("greater than end"), "{err}");
}

#[test]
fn validation_errors_are_located() {
    let src = "[keys]\nzoom = \"prefix+nokey\"\n[[policy.rule]]\nmatch = { command_regex = \"(\" }\neffect = \"deny\"\n[ui.sidebar]\nmin_width = 50\nmax_width = 40\n";
    let err = parse(src).unwrap_err();
    let ConfigError::Invalid(diags) = err else {
        panic!("expected Invalid")
    };
    let find = |needle: &str| diags.iter().find(|d| d.message.contains(needle)).unwrap();
    assert_eq!(find("keys.zoom").line, 2);
    assert_eq!(find("command_regex").line, 4);
    assert_eq!(find("min_width").line, 7);
}

#[test]
fn more_validation() {
    assert!(parse("[notifications]\nquiet_hours = \"22:00-07:00\"\n").is_ok());
    assert!(parse("[notifications]\nquiet_hours = \"late\"\n").is_err());
    assert!(parse("[tasks]\nport_block = 0\n").is_err());
    assert!(parse("[tasks]\nport_pool = \"20000-20004\"\nport_block = 10\n").is_err());
    assert!(parse("[[remote.machine]]\nlabel = \"a\"\naddress = \"x\"\n[[remote.machine]]\nlabel = \"a\"\naddress = \"y\"\n").is_err());
    assert!(parse("[[policy.rule]]\neffect = \"allow\"\n").is_err());
    assert!(parse("[keys]\nprefix = \"nonsense+x\"\n").is_err());
    assert!(parse("[keys]\nprefix_timeout_ms = 0\n").is_err());
}

#[test]
fn warnings_for_shadowing_super_and_conflicts() {
    let src = "[keys]\nhelp = \"ctrl+c\"\nsettings = \"cmd+k\"\ngoto = \"prefix+z\"\n";
    let (_, w) = parse(src).unwrap();
    let msgs: Vec<String> = w.iter().map(|w| w.message.clone()).collect();
    assert!(msgs.iter().any(|m| m.contains("shadows")), "{msgs:?}");
    assert!(msgs.iter().any(|m| m.contains("kitty")), "{msgs:?}");
    assert!(msgs.iter().any(|m| m.contains("conflicts")), "{msgs:?}");
    // the three warnings point at their keys
    assert_eq!(
        w.iter()
            .find(|w| w.message.contains("shadows"))
            .unwrap()
            .pos
            .unwrap()
            .line,
        2
    );
}

#[test]
fn keys_unbind_with_empty_string() {
    let (c, _) = parse("[keys]\nhelp = \"\"\n").unwrap();
    assert_eq!(c.keys.bindings["help"], "");
    assert!(check_keys(&c).is_empty());
}

#[test]
fn missing_file_is_default() {
    let dir = tempfile::tempdir().unwrap();
    let (c, w) = Config::load(dir.path().join("nope.toml")).unwrap();
    assert_eq!(c, Config::default());
    assert!(w.is_empty());
}

#[test]
fn load_reports_file_in_error() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("config.toml");
    std::fs::write(&p, "[ui\n").unwrap();
    let err = Config::load(&p).unwrap_err();
    assert_eq!(err.first_diagnostic().unwrap().file, p);
}

#[test]
fn diff_and_new_pane_classification() {
    let old = Config::default();
    let mut new = old.clone();
    new.theme.name = "nord".into();
    new.ui.sidebar.width = 40;
    new.terminal.term = "xterm-kitty".into();
    new.terminal.env.insert("FOO".into(), "1".into());
    new.keys.bindings.insert("help".into(), "prefix+h".into());
    new.policy.rule.push(PolicyRule {
        matcher: PolicyMatch {
            tool: Some("Bash".into()),
            ..Default::default()
        },
        effect: PolicyEffect::Allow,
        scope: None,
    });
    new.extra
        .insert("preview".into(), toml::Value::Table(Default::default()));
    let d = Config::diff(&old, &new);
    for k in [
        "theme.name",
        "ui.sidebar.width",
        "terminal.term",
        "terminal.env.FOO",
        "keys.help",
        "policy.rule",
        "preview",
    ] {
        assert!(d.contains(&k.to_string()), "{k} not in {d:?}");
    }
    assert_eq!(Config::diff(&old, &old), Vec::<String>::new());

    assert!(requires_new_panes("terminal.term"));
    assert!(requires_new_panes("terminal.default_shell"));
    assert!(requires_new_panes("terminal.shell_mode"));
    assert!(requires_new_panes("terminal.env.FOO"));
    assert!(!requires_new_panes("terminal.scrollback_lines"));
    assert!(!requires_new_panes("theme.name"));
    assert!(!requires_new_panes("keys.help"));
}

#[test]
fn config_path_resolution() {
    let env = |pairs: &'static [(&'static str, &'static str)]| {
        move |k: &str| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    };
    assert_eq!(
        config_path_with(env(&[("VIBEKE_CONFIG", "/x/c.toml"), ("HOME", "/h")])),
        Path::new("/x/c.toml")
    );
    assert_eq!(
        config_path_with(env(&[("XDG_CONFIG_HOME", "/xdg"), ("HOME", "/h")])),
        Path::new("/xdg/vibeke/config.toml")
    );
    assert_eq!(
        config_path_with(env(&[("HOME", "/Users/test")])),
        Path::new("/Users/test/.config/vibeke/config.toml")
    );
    assert_eq!(
        config_path_with(env(&[("XDG_CONFIG_HOME", ""), ("HOME", "/h")])),
        Path::new("/h/.config/vibeke/config.toml")
    );
}

#[test]
fn watcher_reloads_debounced_and_rejects_bad_config() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("config.toml");
    std::fs::write(&p, "[theme]\nname = \"nord\"\n").unwrap();
    let (cur, _) = Config::load(&p).unwrap();
    let (_w, rx) = watch(&p, cur, Duration::from_millis(100)).unwrap();
    std::thread::sleep(Duration::from_millis(200));

    // Burst of writes collapses into one event.
    for name in ["dracula", "gruvbox", "vesper"] {
        std::fs::write(&p, format!("[theme]\nname = \"{name}\"\n")).unwrap();
        std::thread::sleep(Duration::from_millis(10));
    }
    match rx.recv_timeout(Duration::from_secs(10)).unwrap() {
        ReloadEvent::Reloaded {
            config, changed, ..
        } => {
            assert_eq!(config.theme.name, "vesper");
            assert_eq!(changed, vec!["theme.name"]);
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(rx.recv_timeout(Duration::from_millis(400)).is_err());

    // A broken file is rejected, not applied.
    std::fs::write(&p, "[theme\nname = 1\n").unwrap();
    match rx.recv_timeout(Duration::from_secs(10)).unwrap() {
        ReloadEvent::Rejected(e) => assert!(e.first_diagnostic().is_some()),
        other => panic!("unexpected {other:?}"),
    }

    // Fixing it applies, diffed against the last *accepted* config (vesper).
    std::fs::write(&p, "[theme]\nname = \"kanagawa\"\n").unwrap();
    match rx.recv_timeout(Duration::from_secs(10)).unwrap() {
        ReloadEvent::Reloaded {
            config, changed, ..
        } => {
            assert_eq!(config.theme.name, "kanagawa");
            assert_eq!(changed, vec!["theme.name"]);
        }
        other => panic!("unexpected {other:?}"),
    }
}
