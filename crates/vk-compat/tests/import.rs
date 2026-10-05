use vk_compat::*;
use vk_config::{Config, ConfigError};

const REAL_CONFIG: &str = include_str!("fixtures/herdr-config-real.toml");
const FULL_CONFIG: &str = include_str!("fixtures/herdr-config-full.toml");
const REAL_SESSION: &str = include_str!("fixtures/herdr-session.json");
const SPLIT_SESSION: &str = include_str!("fixtures/herdr-session-splits.json");

fn has_mapped(r: &ImportReport, from: &str, to: &str) -> bool {
    r.mapped.iter().any(|(f, t)| f == from && t == to)
}

// ---------------------------------------------------------------- config

#[test]
fn real_config_imports_to_defaults() {
    let r = import_config(REAL_CONFIG);
    assert!(r.errors.is_empty());
    assert_eq!(r.config, Config::default());
    assert!(has_mapped(&r, "onboarding", "onboarding"));
    assert!(r.unsupported.is_empty());
    // Nothing non-default to write.
    assert_eq!(r.toml.lines().filter(|l| !l.starts_with('#')).count(), 0);
    assert!(r.defaulted.contains(&"theme.name".to_string()));
    // Differing Herdr defaults are called out.
    assert!(r.warnings.iter().any(|w| w.contains("ui.sidebar_width")));
}

#[test]
fn full_config_mapping() {
    let r = import_config(FULL_CONFIG);
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let c = &r.config;
    assert_eq!(c.theme.name, "tokyo-night");
    assert!(c.theme.auto_switch);
    assert_eq!(c.theme.dark_name, "tokyo-night");
    assert_eq!(c.theme.custom["panel_bg"], "reset");
    assert_eq!(c.theme.custom["red"], "#ff6188");
    assert_eq!(c.theme.custom["accent"], "#89b4fa"); // from ui.accent
    assert_eq!(c.terminal.default_shell, "/bin/zsh");
    assert_eq!(c.terminal.shell_mode.as_str(), "login");
    assert_eq!(c.terminal.new_cwd, "home");
    assert_eq!(c.update.channel.as_str(), "preview");
    assert!(!c.update.version_check);
    assert!(!c.update.manifest_check);
    assert_eq!(c.keys.prefix, "ctrl+a");
    assert_eq!(c.keys.bindings["new_tab"], "prefix+t");
    assert_eq!(c.keys.bindings["zoom"], "prefix+f"); // fullscreen alias
    assert_eq!(c.keys.bindings["split_vertical"], "prefix+|");
    assert_eq!(c.keys.bindings["remote_image_paste"], "ctrl+shift+v");
    assert_eq!(c.keys.bindings["last_pane"], "prefix+;");
    assert_eq!(c.keys.bindings["switch_tab"], "alt+1..9"); // legacy indexed
    assert_eq!(c.keys.bindings["switch_workspace"], "ctrl+shift+1..9");
    assert_eq!(c.keys.bindings["focus_agent"], ""); // empty legacy value ignored
    assert_eq!(c.keys.navigate["navigate_workspace_up"], "up");
    assert_eq!(c.tasks.root, "~/.herdr/worktrees");
    assert_eq!(c.ui.sidebar.width, 30);
    assert_eq!(c.ui.sidebar.min_width, 20);
    assert_eq!(c.ui.sidebar.max_width, 40);
    assert!(c.ui.sidebar.collapsed);
    assert_eq!(c.ui.confirm_close.as_str(), "never");
    assert_eq!(c.ui.tabs.position.as_str(), "bottom");
    assert!(!c.clipboard.copy_on_select);
    assert_eq!(c.notifications.channel.as_str(), "native");
    assert_eq!(c.notifications.sound, "sounds/notification.mp3");
    assert_eq!(c.agents.resume_on_restart.as_str(), "always");

    // two valid commands carried over; the bad one rejected
    assert_eq!(c.keys.command.len(), 2);
    assert_eq!(c.keys.command[0].command, "lazygit");
    assert_eq!(c.keys.command[0].width.as_deref(), Some("80%"));

    assert!(has_mapped(&r, "keys.fullscreen", "keys.zoom"));
    assert!(has_mapped(&r, "keys.indexed.tabs", "keys.switch_tab"));
    assert!(has_mapped(&r, "worktrees.directory", "tasks.root"));
    assert!(has_mapped(&r, "ui.sidebar_width", "ui.sidebar.width"));
    assert!(has_mapped(&r, "keys.command[1]", "keys.command"));

    let u = r.unsupported.join("\n");
    assert!(u.contains("keys.quit_everything"), "{u}");
    assert!(
        u.contains("keys.command[2]") && u.contains("teleport"),
        "{u}"
    );
    assert!(u.contains("ui.mouse_capture"), "{u}");
    assert!(u.contains("ui.hide_tab_bar_when_single_tab"), "{u}");
    assert!(u.contains("ui.sidebar.agents"), "{u}");
    assert!(u.contains("ui.sound.done_path"), "{u}");
    assert!(u.contains("remote.manage_ssh_config"), "{u}");
    assert!(u.contains("advanced.scrollback_limit_bytes"), "{u}");
    assert!(!u.contains("keys.fullscreen"));

    // sidebar width is explicitly set, so it is not in the "not set" differences
    assert!(!r.warnings.iter().any(|w| w.starts_with("ui.sidebar_width")));
    // Vibeke defaults are not repeated in the output
    assert!(!r.toml.contains("help ="));
    assert!(!r.toml.contains("onboarding"));
}

#[test]
fn imported_toml_round_trips_to_same_config() {
    let r = import_config(FULL_CONFIG);
    let (c, w) = Config::parse(&r.toml, std::path::Path::new("x")).unwrap();
    assert_eq!(c, r.config);
    // The kitty-keyboard and conflict warnings are plain warnings, not errors.
    let _ = w;
}

#[test]
fn invalid_values_are_reported_not_fatal() {
    let r = import_config(
        "[terminal]\nshell_mode = \"fancy\"\n[ui]\nsidebar_width = \"wide\"\nconfirm_close = \"yes\"\n[keys]\nzoom = \"prefix+nokey\"\nhelp = 3\n[theme]\nname = \"nord\"\n",
    );
    assert!(r.errors.is_empty());
    assert_eq!(r.config.theme.name, "nord");
    let u = r.unsupported.join("\n");
    assert!(u.contains("terminal.shell_mode"), "{u}");
    assert!(u.contains("ui.sidebar_width"), "{u}");
    assert!(u.contains("ui.confirm_close"), "{u}");
    assert!(u.contains("keys.zoom"), "{u}");
    assert!(u.contains("keys.help"), "{u}");
    assert_eq!(
        r.config.terminal.shell_mode,
        Config::default().terminal.shell_mode
    );
}

#[test]
fn explicit_action_beats_alias_and_legacy_indexed() {
    let r = import_config(
        "[keys]\nzoom = \"prefix+z\"\nfullscreen = \"prefix+alt+z\"\nswitch_tab = \"prefix+1..5\"\n[keys.indexed]\ntabs = \"ctrl\"\n",
    );
    assert_eq!(r.config.keys.bindings["zoom"], "prefix+z");
    assert_eq!(r.config.keys.bindings["switch_tab"], "prefix+1..5");
    let u = r.unsupported.join("\n");
    assert!(
        u.contains("keys.fullscreen") && u.contains("takes precedence"),
        "{u}"
    );
    assert!(u.contains("keys.indexed.tabs"), "{u}");
}

#[test]
fn toast_and_sound_edge_cases() {
    let r = import_config(
        "[ui.toast]\ndelivery = \"terminal\"\n[ui.sound]\nenabled = false\npath = \"x.mp3\"\n",
    );
    assert_eq!(r.config.notifications.channel.as_str(), "osc");
    assert_eq!(r.config.notifications.sound, "none");
    let r = import_config("[ui.toast]\ndelivery = \"herdr\"\n");
    assert_eq!(r.config.notifications.channel.as_str(), "none");
    let r = import_config("[ui.toast]\ndelivery = \"carrier-pigeon\"\n");
    assert!(
        r.unsupported
            .iter()
            .any(|u| u.contains("ui.toast.delivery"))
    );
}

#[test]
fn explicit_theme_custom_accent_wins_over_ui_accent() {
    let r = import_config("[theme.custom]\naccent = \"#111111\"\n[ui]\naccent = \"cyan\"\n");
    assert_eq!(r.config.theme.custom["accent"], "#111111");
    assert!(r.unsupported.iter().any(|u| u.contains("ui.accent")));
}

#[test]
fn sidebar_token_rules_pass_through() {
    let r = import_config(
        "[[ui.sidebar.token]]\nmatch = { harness = \"codex\" }\nlabel = \"cx\"\n[[ui.sidebar.token]]\nmatch = { regex = \"^tmp\" }\nhide = true\n[[ui.sidebar.token]]\nlabel = \"no match\"\n",
    );
    assert_eq!(r.config.ui.sidebar.token.len(), 2);
    assert!(r.config.ui.sidebar.token[1].hide);
    assert!(
        r.unsupported
            .iter()
            .any(|u| u.contains("ui.sidebar.token[2]"))
    );
}

#[test]
fn garbage_input_is_an_error_report() {
    let r = import_config("this is [not toml");
    assert!(!r.errors.is_empty());
    assert_eq!(r.config, Config::default());
    assert!(r.toml.is_empty());
}

#[test]
fn imported_config_conflicts_surface_as_warnings() {
    let r = import_config("[keys]\nnew_tab = \"prefix+n\"\n");
    assert!(
        r.warnings.iter().any(|w| w.contains("conflicts")),
        "{:?}",
        r.warnings
    );
}

#[test]
fn cmd_bindings_warn_about_kitty_keyboard() {
    let r = import_config("[keys]\nnew_tab = \"cmd+t\"\n");
    assert!(
        r.warnings.iter().any(|w| w.contains("kitty")),
        "{:?}",
        r.warnings
    );
    assert_eq!(r.config.keys.bindings["new_tab"], "cmd+t");
}

// ---------------------------------------------------------------- write rules

#[test]
fn write_imported_never_clobbers_without_force() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("vibeke");

    // Fresh dir: writes config.toml (and creates the directory).
    let p = write_imported(&cfg, "a = 1\n", false).unwrap();
    assert_eq!(p, cfg.join("config.toml"));
    assert_eq!(std::fs::read_to_string(&p).unwrap(), "a = 1\n");

    // Existing config: sidecar file, original untouched.
    let p = write_imported(&cfg, "b = 2\n", false).unwrap();
    assert_eq!(p, cfg.join("config.imported.toml"));
    assert_eq!(std::fs::read_to_string(&p).unwrap(), "b = 2\n");
    assert_eq!(
        std::fs::read_to_string(cfg.join("config.toml")).unwrap(),
        "a = 1\n"
    );

    // Again: the sidecar is replaced (it is ours), the config still untouched.
    write_imported(&cfg, "c = 3\n", false).unwrap();
    assert_eq!(
        std::fs::read_to_string(cfg.join("config.imported.toml")).unwrap(),
        "c = 3\n"
    );
    assert_eq!(
        std::fs::read_to_string(cfg.join("config.toml")).unwrap(),
        "a = 1\n"
    );

    // force overwrites config.toml.
    let p = write_imported(&cfg, "d = 4\n", true).unwrap();
    assert_eq!(p, cfg.join("config.toml"));
    assert_eq!(std::fs::read_to_string(&p).unwrap(), "d = 4\n");

    // No temp files left behind.
    let names: Vec<String> = std::fs::read_dir(&cfg)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(names.iter().all(|n| !n.ends_with(".tmp")), "{names:?}");
}

#[test]
fn written_import_loads_with_vk_config() {
    let dir = tempfile::tempdir().unwrap();
    let r = import_config(FULL_CONFIG);
    let p = write_imported(dir.path(), &r.toml, false).unwrap();
    let (c, _) = Config::load(&p).unwrap();
    assert_eq!(c, r.config);
    // and a corrupted one fails with a location
    std::fs::write(&p, "[ui\n").unwrap();
    assert!(matches!(Config::load(&p), Err(ConfigError::Parse(_))));
}

// ---------------------------------------------------------------- session

#[test]
fn real_session_shape() {
    let plan = import_session(REAL_SESSION).unwrap();
    assert_eq!(plan.version, Some(3));
    assert_eq!(plan.workspaces.len(), 5);
    assert_eq!(plan.active_workspace, Some(2));
    assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);

    let w0 = &plan.workspaces[0];
    assert_eq!(w0.id, "w5");
    assert_eq!(w0.name, "samplehub");
    assert_eq!(w0.cwd, "/Users/test/code/samplehub");
    assert_eq!(w0.tabs.len(), 4);
    assert_eq!(w0.active_tab, 3);
    assert_eq!(w0.tabs[0].number, Some(19));
    assert!(w0.tabs[0].title.is_none());
    let Layout::Leaf(l) = &w0.tabs[0].layout else {
        panic!("expected a single pane")
    };
    assert_eq!(l.cwd, "/Users/test/code/samplehub");
    assert!(l.focused);
    assert_eq!(l.agent.as_ref().unwrap().harness, "codex");

    // every pane in the file has a cwd under /Users/test (sanitised fixture)
    for w in &plan.workspaces {
        for t in &w.tabs {
            for l in t.layout.leaves() {
                assert!(l.cwd.starts_with("/Users/test"), "{}", l.cwd);
            }
        }
    }
}

#[test]
fn real_session_resume_candidates() {
    let plan = import_session(REAL_SESSION).unwrap();
    let c = plan.resume_candidates();
    assert_eq!(c.len(), 9);
    let claude = c.iter().filter(|c| c.agent.harness == "claude").count();
    let codex = c.iter().filter(|c| c.agent.harness == "codex").count();
    assert_eq!((claude, codex), (7, 2));
    for cand in &c {
        let argv = cand.argv.as_ref().unwrap();
        match cand.agent.harness.as_str() {
            "claude" => assert_eq!(argv[..2], ["claude", "--resume"]),
            "codex" => assert_eq!(argv[..2], ["codex", "resume"]),
            other => panic!("unexpected harness {other}"),
        }
        assert_eq!(argv[2], cand.agent.session_id);
        assert!(
            cand.agent
                .session_id
                .starts_with("00000000-0000-4000-8000-")
        );
    }
}

#[test]
fn split_layouts_and_edge_cases() {
    let plan = import_session(SPLIT_SESSION).unwrap();
    assert_eq!(plan.workspaces.len(), 2);

    let w = &plan.workspaces[0];
    assert_eq!(w.name, "api server");
    assert_eq!(w.tabs[0].title.as_deref(), Some("dev"));
    assert!(w.tabs[0].zoomed);
    let Layout::Split {
        orientation,
        ratio,
        first,
        second,
    } = &w.tabs[0].layout
    else {
        panic!("expected split")
    };
    assert_eq!(*orientation, Orientation::SideBySide);
    assert!((ratio - 0.6).abs() < 1e-6);
    assert!(matches!(**first, Layout::Leaf(ref l) if l.pane_id == 1));
    let Layout::Split {
        orientation: o2,
        ratio: r2,
        ..
    } = &**second
    else {
        panic!("expected nested split")
    };
    assert_eq!(*o2, Orientation::Stacked);
    assert!((r2 - 0.5).abs() < 1e-6);

    let leaves = w.tabs[0].layout.leaves();
    assert_eq!(
        leaves.iter().map(|l| l.pane_id).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert_eq!(leaves[1].cwd, "/Users/test/code/api/web");
    assert!(leaves[1].focused && !leaves[0].focused);
    assert!(leaves[1].agent.is_none());

    // Resume: claude, codex, pi (path), gemini (no argv).
    let c = plan.resume_candidates();
    assert_eq!(c.len(), 4);
    assert_eq!(
        c[0].argv.as_deref().unwrap(),
        ["claude", "--resume", "11111111-1111-4111-8111-111111111111"]
    );
    assert_eq!(
        c[1].argv.as_deref().unwrap(),
        ["codex", "resume", "22222222-2222-4222-8222-222222222222"]
    );
    assert_eq!(
        c[2].argv.as_deref().unwrap(),
        ["pi", "--session", "/Users/test/.pi/sessions/abc.jsonl"]
    );
    assert_eq!(c[2].agent.kind, SessionRefKind::Path);
    assert_eq!(c[3].agent.harness, "gemini");
    assert!(c[3].argv.is_none());
    assert_eq!(c[3].workspace, "docs");

    // Second workspace: out-of-range active_tab and a pane without a record.
    let w2 = &plan.workspaces[1];
    assert_eq!(w2.active_tab, 0);
    let Layout::Leaf(l) = &w2.tabs[1].layout else {
        panic!()
    };
    assert_eq!(l.cwd, "/Users/test/code/docs"); // falls back to the workspace cwd
    let warns = plan.warnings.join("\n");
    assert!(warns.contains("active_tab 5 out of range"), "{warns}");
    assert!(warns.contains("pane 77 has no record"), "{warns}");
    assert_eq!(plan.active_workspace, Some(1));
}

#[test]
fn alternative_layout_spellings() {
    let json = r#"{"version":3,"workspaces":[{"id":"w1","identity_cwd":"/Users/test/x","tabs":[{
        "layout":{"type":"split","direction":"down","ratio":0.25,
                  "first":{"type":"pane","pane_id":1},"second":{"type":"pane","pane_id":2}},
        "panes":{"1":{"cwd":"/a"},"2":{"cwd":"/b"}},"focused":2}]}]}"#;
    let plan = import_session(json).unwrap();
    let Layout::Split {
        orientation, ratio, ..
    } = &plan.workspaces[0].tabs[0].layout
    else {
        panic!()
    };
    assert_eq!(*orientation, Orientation::Stacked);
    assert!((ratio - 0.25).abs() < 1e-6);
    assert_eq!(plan.workspaces[0].name, "x");
}

#[test]
fn bad_sessions() {
    assert!(matches!(
        import_session("{"),
        Err(SessionImportError::Json(_))
    ));
    assert!(matches!(
        import_session("{\"version\":3}"),
        Err(SessionImportError::NoWorkspaces)
    ));
    let p = import_session(r#"{"version":9,"workspaces":[]}"#).unwrap();
    assert!(p.workspaces.is_empty());
    assert!(p.warnings.iter().any(|w| w.contains("version")));
    // unreadable layout is skipped with a warning, not a failure
    let p = import_session(
        r#"{"version":3,"workspaces":[{"id":"w","identity_cwd":"/","tabs":[{"layout":{"Bogus":1},"panes":{}}]}]}"#,
    )
    .unwrap();
    assert!(p.workspaces[0].tabs.is_empty());
    assert!(p.warnings.iter().any(|w| w.contains("unreadable layout")));
    // empty agent_session value is ignored
    let p = import_session(
        r#"{"version":3,"workspaces":[{"id":"w","identity_cwd":"/a/b","tabs":[{"layout":{"Pane":1},"panes":{"1":{"cwd":"/a/b","agent_session":{"agent":"claude","kind":"id","value":""}}}}]}]}"#,
    )
    .unwrap();
    assert!(p.resume_candidates().is_empty());
}
