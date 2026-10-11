use super::*;

#[test]
fn a_valid_config_is_ok() {
    assert!(check("").is_empty());
    assert!(
        check(DEFAULT_CONFIG).is_empty(),
        "{:?}",
        check(DEFAULT_CONFIG)
    );
    let cfg = r#"
onboarding = false
[keys]
prefix = "ctrl+a"
new_tab = ["prefix+c", "alt+t"]
switch_tab = "prefix+1..9"
navigate_pane_left = "h"
fullscreen = "prefix+z"

[keys.indexed]
tabs = "alt"

[[keys.command]]
key = "prefix+shift+z"
command = "my-tool --toggle"
type = "popup"
width = "80%"
height = 20

[ui]
sidebar_width = 30
"#;
    assert!(check(cfg).is_empty(), "{:?}", check(cfg));
    assert_eq!(report(&[]), ("config: ok\n".to_string(), 0));
}

#[test]
fn syntax_and_type_errors_fail_the_whole_file() {
    let d = check("[keys\nprefix = 1");
    assert_eq!(d.len(), 1);
    assert!(d[0].starts_with("config parse error: "), "{d:?}");
    assert!(d[0].ends_with("; using defaults"), "{d:?}");
    let d = check("[keys]\nnew_tab = 3\n");
    assert_eq!(d.len(), 1, "{d:?}");
    assert!(d[0].starts_with("config parse error: "), "{d:?}");
    let d = check("[[keys.command]]\nkey = \"prefix+g\"\ncommand = \"x\"\ntype = \"window\"\n");
    assert!(d[0].starts_with("config parse error: "), "{d:?}");
    let d = check("keys = \"x\"\n");
    assert!(d[0].starts_with("config parse error: "), "{d:?}");
    let (out, code) = report(&d);
    assert_eq!(code, 1);
    assert!(out.starts_with("config: issues found\nconfig parse error: "));
}

#[test]
fn unknown_sections_and_keys() {
    let d = check(
        "stray = 1\n[toast]\nx = 1\n[bogus]\n[keys]\nsplit_sideways = \"prefix+s\"\n[keys.indexed]\nrows = \"alt\"\n",
    );
    assert!(
        d.contains(
            &"unknown config section [toast]; did you mean [ui.toast]? ignoring section"
                .to_string()
        ),
        "{d:?}"
    );
    assert!(
        d.contains(&"unknown config section [bogus]; ignoring section".to_string()),
        "{d:?}"
    );
    assert!(
        d.contains(&"unknown config key keys.indexed.rows; ignoring key".to_string()),
        "{d:?}"
    );
    assert!(
        d.contains(&"unknown config key keys.split_sideways; ignoring key".to_string()),
        "{d:?}"
    );
    assert!(
        d.contains(&"unknown config key stray; ignoring key".to_string()),
        "{d:?}"
    );
    let d = check("[[keys.command]]\nkey = \"prefix+g\"\ncommand = \"x\"\nicon = \"*\"\n");
    assert_eq!(
        d,
        vec!["unknown config key keys.command.0.icon; ignoring key".to_string()]
    );
}

#[test]
fn keybinding_syntax() {
    let d = check("[keys]\nnew_tab = \"prefix+ctrl+shift\"\n");
    assert_eq!(
        d,
        vec![
            r#"invalid keybinding: keys.new_tab = "prefix+ctrl+shift"; disabling binding"#
                .to_string()
        ]
    );
    let d = check("[keys]\nnew_tab = \"alt+1..9\"\n");
    assert_eq!(
        d,
        vec![r#"range keybinding is only valid for indexed actions: keys.new_tab = "alt+1..9"; disabling binding"#.to_string()]
    );
    let d = check("[keys]\nprefix = \"nope+x+y\"\n");
    assert_eq!(
        d,
        vec![
            "invalid keybinding: keys.prefix; using fallback ctrl+b".to_string(),
            r#"invalid keybinding: keys.prefix = "nope+x+y"; ignoring prefix"#.to_string(),
        ]
    );
    let d = check("[keys]\nnavigate_pane_left = \"prefix+h\"\n");
    assert!(
        d[0].starts_with("navigate keybinding must not include prefix"),
        "{d:?}"
    );
    let d = check("[keys]\nswitch_tab = \"prefix+x\"\n");
    assert!(
        d[0].starts_with("indexed keybinding must use 1..9"),
        "{d:?}"
    );
    let d = check("[keys.indexed]\ntabs = \"alt+x\"\n");
    assert_eq!(
        d,
        vec![
            r#"invalid indexed keybinding: keys.indexed.tabs = "alt+x"; disabling binding"#
                .to_string()
        ]
    );
    let d = check("[keys]\nremote_image_paste = \"ctrl+\"\n");
    assert!(
        d[0].starts_with("invalid keybinding: keys.remote_image_paste"),
        "{d:?}"
    );
    // Keys Herdr accepts.
    for ok in [
        "f12",
        "esc",
        "shift+tab",
        "ctrl+space",
        "prefix+minus",
        "cmd+K",
        "prefix+?",
    ] {
        assert!(
            check(&format!("[keys]\nhelp = \"{ok}\"\n")).is_empty(),
            "{ok}"
        );
    }
}

#[test]
fn custom_commands() {
    let d = check("[[keys.command]]\nkey = \"prefix+g\"\ncommand = \"  \"\n");
    assert_eq!(
        d,
        vec!["empty custom command: keys.command[0].command; disabling custom command".to_string()]
    );
    let d = check("[[keys.command]]\nkey = \"prefix+g\"\ncommand = \"x\"\nwidth = 40\n");
    assert_eq!(
        d,
        vec![
            "popup size on non-popup custom command: keys.command[0]; ignoring width and height"
                .to_string()
        ]
    );
    let d = check("[[keys.command]]\nkey = \"prefix+\"\ncommand = \"x\"\n");
    assert_eq!(
        d,
        vec![
            r#"invalid keybinding: keys.command[0].key = "prefix+"; disabling binding"#.to_string()
        ]
    );
}

#[test]
fn files() {
    let dir = tempfile::tempdir().unwrap();
    assert!(check_file(&dir.path().join("missing.toml")).is_empty());
    let f = dir.path().join("config.toml");
    std::fs::write(&f, "\u{feff}[keys]\nprefix = \"ctrl+a\"\n").unwrap();
    assert!(check_file(&f).is_empty());
    std::fs::write(&f, "[keys\n").unwrap();
    assert_eq!(check_file(&f).len(), 1);
}
