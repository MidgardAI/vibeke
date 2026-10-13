use super::*;
use crate::drafts::tests::{commands, fleet, only, screen};
use crate::formkeys;
use vk_proto::input::Mods;

fn key(app: &mut App, k: Key) {
    app.on_key(KeyEvent::new(k, Mods::empty()));
}
fn enter(app: &mut App) {
    key(app, Key::Named(NamedKey::Enter));
}

/// Harness config dirs inside a temp dir: never the user's real configs.
fn temp_dirs(d: &Path) -> Dirs {
    Dirs {
        claude: d.join("claude"),
        codex: d.join("codex"),
        pi: d.join("pi"),
        omp: d.join("omp"),
        opencode: d.join("opencode"),
        gemini: d.join("gemini"),
        agents: d.join("agents"),
        hermes: d.join("hermes"),
    }
}

#[test]
fn starter_config_writes_only_non_default_choices() {
    let c = Choices {
        notifications: Some("osc".into()),
        theme: Some("catppuccin".into()),
        theme_mode: Some("auto".into()),
        herdr_toml: None,
    };
    let t = starter_config(None, &c).unwrap();
    assert!(t.contains("onboarding = false"), "{t}");
    assert!(t.contains("channel = \"osc\""), "{t}");
    assert!(!t.contains("[theme]"), "defaults are not written: {t}");
    // An existing file keeps its comments and keys; a choice back to the default is removed.
    let existing = "# mine\n[theme]\nname = \"terminal\" # keep me?\n[ui]\nanimate = false\n";
    let c2 = Choices {
        notifications: Some("native".into()),
        theme: Some("catppuccin".into()),
        ..Default::default()
    };
    let t2 = starter_config(Some(existing), &c2).unwrap();
    assert!(t2.contains("# mine"), "{t2}");
    assert!(t2.contains("animate = false"), "{t2}");
    assert!(!t2.contains("name = \"terminal\""), "{t2}");
    assert!(!t2.contains("[notifications]"), "{t2}");
    // Imported Herdr keys are the base when there is no file yet.
    let c3 = Choices {
        herdr_toml: Some("[keys]\nprefix = \"ctrl+a\"\n".into()),
        ..Default::default()
    };
    let t3 = starter_config(None, &c3).unwrap();
    assert!(t3.contains("prefix = \"ctrl+a\"") && t3.contains("onboarding = false"));
    let (cfg, _) = vk_config::Config::parse(&t3, Path::new("x")).unwrap();
    assert_eq!(cfg.keys.prefix, "ctrl+a");
    // Invalid results are refused, never written.
    assert!(starter_config(Some("[ui]\nmax_fps = \"fast\"\n"), &Choices::default()).is_err());
}

#[test]
fn terminal_table_reflects_the_probe() {
    let mut caps = HostCaps::default();
    let rows = terminal_checks(&caps, false, false);
    assert!(!rows.iter().find(|c| c.name == "keyboard").unwrap().ok);
    caps.kitty_graphics = true;
    caps.truecolor = true;
    let rows = terminal_checks(&caps, true, true);
    for n in ["keyboard", "graphics", "clipboard", "colour"] {
        assert!(rows.iter().find(|c| c.name == n).unwrap().ok, "{n}");
    }
}

#[test]
fn walks_every_step_installs_only_after_confirmation_and_writes_the_config() {
    let tmp = tempfile::tempdir().unwrap();
    let dirs = temp_dirs(tmp.path());
    let cfg_path = tmp.path().join("vibeke/config.toml");
    let (mut app, mut rxs) = fleet();
    open_with(&mut app, dirs.clone(), cfg_path.clone());
    {
        let f = app.ux.onboarding.as_mut().unwrap();
        f.herdr = None; // hermetic: ignore a real ~/.config/herdr
        for r in &mut f.harnesses {
            r.selected = false;
        }
    }
    assert!(matches!(app.mode, Mode::Popup(Popup::Onboarding)));
    let s = screen(&app);
    assert!(s.contains("Welcome to Vibeke · setup 1/5"), "{s}");
    assert!(s.contains("Your terminal") && s.contains("keyboard"), "{s}");
    enter(&mut app);
    assert_eq!(app.ux.onboarding.as_ref().unwrap().step, Step::Integrations);
    let s = screen(&app);
    assert!(s.contains("claude") && s.contains("not installed"), "{s}");
    // `i` with nothing selected installs nothing.
    key(&mut app, Key::Char('i'));
    assert!(!app.ux.onboarding.as_ref().unwrap().confirm_install);
    // Select claude (row 0), look at the diff, ask to install, cancel.
    key(&mut app, Key::Char(' '));
    key(&mut app, Key::Char('d'));
    let s = screen(&app);
    assert!(
        s.contains("claude changes") && s.contains("settings.json"),
        "{s}"
    );
    key(&mut app, Key::Char('x')); // closes the diff
    key(&mut app, Key::Char('i'));
    let s = screen(&app);
    assert!(s.contains("Install into these files?"), "{s}");
    assert!(
        s.contains(
            &tmp.path()
                .join("claude/settings.json")
                .display()
                .to_string()
        )
    );
    key(&mut app, Key::Char('n'));
    assert!(
        !tmp.path().join("claude/settings.json").exists(),
        "cancelled"
    );
    // Now confirm: written into the temp dir only.
    if !app.ux.onboarding.as_ref().unwrap().harnesses[0].selected {
        key(&mut app, Key::Char(' '));
    }
    key(&mut app, Key::Char('i'));
    key(&mut app, Key::Char('y'));
    let settings = tmp.path().join("claude/settings.json");
    assert!(settings.exists());
    assert!(std::fs::read_to_string(&settings).unwrap().contains("hook"));
    let f = app.ux.onboarding.as_ref().unwrap();
    assert!(
        f.harnesses[0]
            .result
            .as_deref()
            .unwrap()
            .starts_with("wrote ")
    );
    // Notifications: terminal, with a test notification.
    enter(&mut app);
    key(&mut app, Key::Char('2'));
    key(&mut app, Key::Char('t'));
    let cmds = commands(&mut rxs[0]);
    let (_, p) = only(&cmds, "notification.send");
    assert_eq!(p["title"], "Vibeke test notification");
    enter(&mut app);
    // Theme: latte, previewed live.
    key(&mut app, Key::Char('j'));
    assert_eq!(
        format!("{:?}", app.theme.fg),
        format!("{:?}", crate::theme::Theme::latte().fg)
    );
    enter(&mut app);
    assert_eq!(app.ux.onboarding.as_ref().unwrap().step, Step::Write);
    let s = screen(&app);
    assert!(
        s.contains("onboarding = false") && s.contains("channel = \"osc\""),
        "{s}"
    );
    assert!(!cfg_path.exists());
    enter(&mut app);
    let text = std::fs::read_to_string(&cfg_path).unwrap();
    let (cfg, _) = vk_config::Config::parse(&text, &cfg_path).unwrap();
    assert!(!cfg.onboarding);
    assert_eq!(cfg.notifications.channel.as_str(), "osc");
    assert_eq!(cfg.theme.name, "catppuccin-latte");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&cfg_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(!app.config.onboarding);
    // enter again closes.
    enter(&mut app);
    assert!(matches!(app.mode, Mode::Normal));
}

#[test]
fn esc_closes_for_now_and_setup_reopens() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut app, _rx) = fleet();
    open_with(&mut app, temp_dirs(tmp.path()), tmp.path().join("c.toml"));
    key(&mut app, Key::Named(NamedKey::Escape));
    assert!(matches!(app.mode, Mode::Normal));
    assert!(app.toasts.last().unwrap().text.contains(":setup"));
    assert!(!tmp.path().join("c.toml").exists());
    // `tab` / shift+tab move between steps.
    open_with(&mut app, temp_dirs(tmp.path()), tmp.path().join("c.toml"));
    app.ux.onboarding.as_mut().unwrap().herdr = None;
    key(&mut app, Key::Named(NamedKey::Tab));
    assert_eq!(app.ux.onboarding.as_ref().unwrap().step, Step::Integrations);
    app.on_key(KeyEvent::new(Key::Named(NamedKey::Tab), Mods::SHIFT));
    assert_eq!(app.ux.onboarding.as_ref().unwrap().step, Step::Terminal);
}

#[test]
fn herdr_step_imports_into_the_written_config() {
    let tmp = tempfile::tempdir().unwrap();
    let herdr = tmp.path().join("herdr.toml");
    std::fs::write(&herdr, "[keys]\nprefix = \"ctrl+a\"\n").unwrap();
    let cfg_path = tmp.path().join("config.toml");
    let (mut app, _rx) = fleet();
    open_with(&mut app, temp_dirs(tmp.path()), cfg_path.clone());
    app.ux.onboarding.as_mut().unwrap().herdr = Some(herdr);
    enter(&mut app);
    assert_eq!(app.ux.onboarding.as_ref().unwrap().step, Step::Herdr);
    assert!(screen(&app).contains("Import keybindings"));
    key(&mut app, Key::Char('y'));
    let f = app.ux.onboarding.as_ref().unwrap();
    assert!(f.choices.herdr_toml.as_deref().unwrap().contains("ctrl+a"));
    assert!(f.preview().unwrap().contains("prefix = \"ctrl+a\""));
}

fn named(app: &mut App, n: NamedKey) {
    key(app, Key::Named(n));
}

/// A setup view on `step` with no Herdr step and nothing selected, config in a temp dir.
fn at(step: Step, tmp: &Path) -> App {
    let (mut app, _rx) = fleet();
    open_with(&mut app, temp_dirs(tmp), tmp.join("c.toml"));
    let f = app.ux.onboarding.as_mut().unwrap();
    f.herdr = None;
    for r in &mut f.harnesses {
        r.selected = false;
    }
    while f.step != step {
        f.go(1);
    }
    app
}

fn flow(app: &App) -> &Flow {
    app.ux.onboarding.as_ref().unwrap()
}

#[test]
fn up_from_the_top_row_focuses_the_strip_and_arrows_switch_steps() {
    let tmp = tempfile::tempdir().unwrap();
    let mut app = at(Step::Notifications, tmp.path());
    named(&mut app, NamedKey::Up);
    assert!(flow(&app).strip.focused);
    let s = screen(&app);
    assert!(s.contains("‹ notifications ›"), "{s}");
    assert!(s.contains(formkeys::STRIP_HINT), "{s}");
    named(&mut app, NamedKey::Right);
    assert_eq!(flow(&app).step, Step::Theme);
    assert!(flow(&app).strip.focused);
    named(&mut app, NamedKey::Left);
    named(&mut app, NamedKey::Left);
    assert_eq!(flow(&app).step, Step::Integrations);
    // esc goes back into the step; it does not close setup.
    named(&mut app, NamedKey::Escape);
    assert!(!flow(&app).strip.focused);
    assert!(matches!(app.mode, Mode::Popup(Popup::Onboarding)));
    // ↓ and enter go back too.
    named(&mut app, NamedKey::Up);
    named(&mut app, NamedKey::Down);
    assert!(!flow(&app).strip.focused);
    assert_eq!(flow(&app).step, Step::Integrations);
    named(&mut app, NamedKey::Up);
    enter(&mut app);
    assert!(!flow(&app).strip.focused);
    assert_eq!(flow(&app).step, Step::Integrations);
}

#[test]
fn up_below_the_top_row_only_moves_and_tab_still_switches_steps() {
    let tmp = tempfile::tempdir().unwrap();
    let mut app = at(Step::Notifications, tmp.path());
    named(&mut app, NamedKey::Down);
    named(&mut app, NamedKey::Up);
    assert!(!flow(&app).strip.focused);
    assert_eq!(flow(&app).sel, 0);
    named(&mut app, NamedKey::Tab);
    assert_eq!(flow(&app).step, Step::Theme);
    // From a step with no list, ↑ goes straight to the strip.
    let mut app = at(Step::Terminal, tmp.path());
    named(&mut app, NamedKey::Up);
    assert!(flow(&app).strip.focused);
    // Tab on the strip switches steps too.
    named(&mut app, NamedKey::Tab);
    assert_eq!(flow(&app).step, Step::Integrations);
}

#[test]
fn arrows_choose_the_notification_option() {
    let tmp = tempfile::tempdir().unwrap();
    let mut app = at(Step::Notifications, tmp.path());
    named(&mut app, NamedKey::Right);
    assert_eq!(flow(&app).choices.notifications.as_deref(), Some("osc"));
    named(&mut app, NamedKey::Right);
    assert_eq!(flow(&app).choices.notifications.as_deref(), Some("none"));
    named(&mut app, NamedKey::Left);
    assert_eq!(flow(&app).choices.notifications.as_deref(), Some("osc"));
    assert_eq!(flow(&app).sel, 1);
    // ↓ moves the cursor without choosing; space chooses it.
    named(&mut app, NamedKey::Down);
    assert_eq!(flow(&app).choices.notifications.as_deref(), Some("osc"));
    key(&mut app, Key::Char(' '));
    assert_eq!(flow(&app).choices.notifications.as_deref(), Some("none"));
    // The digit shortcuts still work.
    key(&mut app, Key::Char('1'));
    assert_eq!(flow(&app).choices.notifications.as_deref(), Some("native"));
}

#[test]
fn arrows_choose_the_theme_and_toggle_auto() {
    let tmp = tempfile::tempdir().unwrap();
    let mut app = at(Step::Theme, tmp.path());
    let start = flow(&app).sel;
    named(&mut app, NamedKey::Right);
    assert_eq!(flow(&app).sel, (start + 1) % THEMES.len());
    assert_eq!(
        flow(&app).choices.theme.as_deref(),
        Some(THEMES[(start + 1) % THEMES.len()])
    );
    named(&mut app, NamedKey::Left);
    assert_eq!(flow(&app).choices.theme.as_deref(), Some(THEMES[start]));
    // The auto toggle is the row after the themes: ←/→/space flip it, `a` still does.
    for _ in 0..THEMES.len() {
        named(&mut app, NamedKey::Down);
    }
    assert_eq!(flow(&app).sel, THEMES.len());
    let auto = |app: &App| flow(app).choices.theme_mode.clone();
    let before = auto(&app);
    named(&mut app, NamedKey::Right);
    assert_ne!(auto(&app), before);
    named(&mut app, NamedKey::Left);
    assert_eq!(auto(&app), before);
    key(&mut app, Key::Char(' '));
    assert_ne!(auto(&app), before);
    key(&mut app, Key::Char('a'));
    assert_eq!(auto(&app), before);
    // The theme choice is untouched by the toggle.
    assert_eq!(flow(&app).choices.theme.as_deref(), Some(THEMES[start]));
}

#[test]
fn arrows_set_an_integration_checkbox_on_or_off() {
    let tmp = tempfile::tempdir().unwrap();
    let mut app = at(Step::Integrations, tmp.path());
    assert!(!flow(&app).harnesses[0].files.is_empty());
    named(&mut app, NamedKey::Right);
    assert!(flow(&app).harnesses[0].selected);
    named(&mut app, NamedKey::Right);
    assert!(flow(&app).harnesses[0].selected, "→ sets on, not flip");
    named(&mut app, NamedKey::Left);
    assert!(!flow(&app).harnesses[0].selected);
    key(&mut app, Key::Char(' '));
    assert!(flow(&app).harnesses[0].selected);
    key(&mut app, Key::Char('h'));
    assert!(!flow(&app).harnesses[0].selected);
}
