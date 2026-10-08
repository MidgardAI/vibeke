//! First-run onboarding and `:setup` (08 §9). The same steps back `vibeke setup` (CLI).
//!
//! Steps: 1 terminal check (pass/warn table from the startup probe), 2 Herdr import (when
//! `~/.config/herdr/config.toml` exists), 3 agent integrations (detected harnesses with their
//! install state; the exact file diff is shown before anything is written, and installing needs
//! an explicit key plus a `y` confirmation naming the files), 4 notifications (native / terminal /
//! none, with a test notification), 5 theme (built-ins, auto light/dark), 6 write the config:
//! only the non-default choices plus `onboarding = false`, merged into an existing file with
//! `toml_edit` so the user's comments and keys survive.
//!
//! The view opens by itself only when `onboarding = true` or no config file exists, on a client
//! of the local machine; `esc` closes it for this run (it comes back next time until the config
//! is written), `:setup` reopens it any time.

use crate::app::{App, Mode, Pending, Popup};
use crate::screen::{Grid, HostCaps};
use serde_json::json;
use std::path::{Path, PathBuf};
use vk_agents::{Dirs, Harness, InstallState};
use vk_proto::input::{Key, KeyEvent, KeyKind, NamedKey};

/// One row of the terminal table.
#[derive(Debug, Clone, PartialEq)]
pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

/// Pass/warn table from the host capability probe (03 §6.1).
pub fn terminal_checks(caps: &HostCaps, kitty_keyboard: bool, osc52: bool) -> Vec<Check> {
    let notif = match caps.notifications {
        crate::caps::Notifications::None => None,
        n => Some(format!("{n:?}").to_lowercase()),
    };
    vec![
        Check {
            name: "keyboard",
            ok: kitty_keyboard,
            detail: if kitty_keyboard {
                "kitty keyboard protocol (ctrl+enter, ctrl+shift+p work)".into()
            } else {
                "legacy keys: use alt+enter where ctrl+enter is documented".into()
            },
        },
        Check {
            name: "graphics",
            ok: caps.kitty_graphics || caps.iterm2_images,
            detail: if caps.kitty_graphics {
                "kitty graphics (browser panes, thumbnails)".into()
            } else if caps.iterm2_images {
                "iTerm2 images (browser panes as whole frames)".into()
            } else {
                "none: previews open in a browser window".into()
            },
        },
        Check {
            name: "clipboard",
            ok: osc52,
            detail: if osc52 {
                "OSC 52 copy".into()
            } else {
                "no OSC 52 answer: local copies use pbcopy/wl-copy/xclip".into()
            },
        },
        Check {
            name: "notifications",
            ok: notif.is_some() || native_notifier().is_some(),
            detail: match (notif, native_notifier()) {
                (_, Some(n)) => format!("native via {n}"),
                (Some(o), None) => format!("terminal ({o})"),
                (None, None) => "none found: toasts only".into(),
            },
        },
        Check {
            name: "colour",
            ok: caps.truecolor,
            detail: if caps.truecolor {
                "truecolor".into()
            } else {
                "256 colours".into()
            },
        },
        Check {
            name: "sync updates",
            ok: caps.sync_update,
            detail: if caps.sync_update {
                "DEC 2026 (no tearing)".into()
            } else {
                "not supported (minor flicker)".into()
            },
        },
    ]
}

fn on_path(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(bin))
        .find(|c| c.is_file())
}

/// The native notifier the server would use (08 §7.1), if one is installed.
pub fn native_notifier() -> Option<&'static str> {
    if cfg!(target_os = "macos") {
        if on_path("terminal-notifier").is_some() {
            Some("terminal-notifier")
        } else if on_path("osascript").is_some() {
            Some("osascript")
        } else {
            None
        }
    } else {
        on_path("notify-send").map(|_| "notify-send")
    }
}

/// A detected harness and what installing its integration would change.
#[derive(Debug, Clone)]
pub struct HarnessRow {
    pub harness: Harness,
    pub binary: Option<PathBuf>,
    pub state: InstallState,
    /// Unified diff of the planned install (empty when nothing changes).
    pub diff: String,
    pub files: Vec<PathBuf>,
    pub selected: bool,
    /// Outcome after installing (`wrote …` / the error).
    pub result: Option<String>,
}

impl HarnessRow {
    pub fn state_text(&self) -> &'static str {
        match self.state {
            InstallState::Installed => "installed",
            InstallState::Partial => "partial",
            InstallState::NotInstalled => "not installed",
        }
    }
}

/// The binary a harness runs as.
pub fn harness_bin(h: Harness) -> &'static str {
    match h {
        Harness::Claude => "claude",
        Harness::Codex => "codex",
        Harness::Pi => "pi",
        Harness::Omp => "omp",
        Harness::OpenCode => "opencode",
        Harness::Gemini => "gemini",
    }
}

/// The `vibeke` the hooks call: the stable install when present, else this binary.
pub fn vibeke_bin() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let exe = std::env::current_exe().unwrap_or_else(|_| home.join(".local/bin/vibeke"));
    vk_agents::hook_bin(&exe, &home)
}

/// Every known harness: binary on `PATH`, integration state in `dirs`, and the install plan's
/// diff. Harnesses found on `PATH` without a complete integration start selected.
pub fn detect_harnesses(dirs: &Dirs, bin: &Path) -> Vec<HarnessRow> {
    Harness::ALL
        .iter()
        .map(|&h| {
            let binary = on_path(harness_bin(h));
            let st = vk_agents::status(h, dirs);
            // Hooks pointing at a build output / missing binary are refreshed like a partial
            // install.
            let state = st.state;
            let stale = vk_agents::stale_command(&st);
            let (diff, files) = match vk_agents::plan_install(h, dirs, bin) {
                Ok(p) => {
                    let changed: Vec<_> = p.files.iter().filter(|f| f.changed()).collect();
                    (
                        changed
                            .iter()
                            .map(|f| format!("--- {}\n{}", f.path.display(), f.diff()))
                            .collect::<Vec<_>>()
                            .join("\n"),
                        changed.iter().map(|f| f.path.clone()).collect(),
                    )
                }
                Err(e) => (format!("cannot plan: {e:#}"), vec![]),
            };
            HarnessRow {
                harness: h,
                selected: binary.is_some()
                    && (state != InstallState::Installed || stale)
                    && !files.is_empty(),
                binary,
                state,
                diff,
                files,
                result: None,
            }
        })
        .collect()
}

/// Install one harness integration (re-planned at install time, so the files written are the
/// current plan). Returns what was written.
pub fn install(h: Harness, dirs: &Dirs, bin: &Path) -> Result<Vec<PathBuf>, String> {
    let plan = vk_agents::plan_install(h, dirs, bin).map_err(|e| format!("{e:#}"))?;
    if !plan.changed() {
        return Ok(vec![]);
    }
    vk_agents::apply(&plan).map_err(|e| format!("{e:#}"))
}

/// `~/.config/herdr/config.toml`, when present.
pub fn herdr_config() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let p = home.join(".config/herdr/config.toml");
    p.is_file().then_some(p)
}

/// Choices that end up in the written config.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Choices {
    /// `notifications.channel` (`native`, `osc`, `none`).
    pub notifications: Option<String>,
    /// `theme.name`.
    pub theme: Option<String>,
    /// `theme.mode`: `auto` follows the host's light/dark.
    pub theme_mode: Option<String>,
    /// Imported Herdr config text (only its non-default keys).
    pub herdr_toml: Option<String>,
}

/// Built-in chrome themes offered in step 5.
pub const THEMES: &[&str] = &["catppuccin", "catppuccin-latte", "terminal"];

/// The config text to write: `existing` (or the imported Herdr keys) with the non-default
/// choices set and `onboarding = false`. Comments and other keys of `existing` are kept.
pub fn starter_config(existing: Option<&str>, c: &Choices) -> Result<String, String> {
    let base = match (existing, &c.herdr_toml) {
        (Some(e), Some(h)) if e.trim().is_empty() => h.clone(),
        (Some(e), _) => e.to_string(),
        (None, Some(h)) => h.clone(),
        (None, None) => String::new(),
    };
    let mut doc: toml_edit::DocumentMut = base.parse().map_err(|e| format!("{e}"))?;
    let def = vk_config::Config::default();
    doc["onboarding"] = toml_edit::value(false);
    let mut set = |table: &str, key: &str, v: &str, default: &str| {
        if v == default {
            if let Some(t) = doc.get_mut(table).and_then(|t| t.as_table_like_mut()) {
                t.remove(key);
            }
            return;
        }
        if doc.get(table).is_none() {
            doc[table] = toml_edit::table();
        }
        doc[table][key] = toml_edit::value(v);
    };
    if let Some(n) = &c.notifications {
        set(
            "notifications",
            "channel",
            n,
            def.notifications.channel.as_str(),
        );
    }
    if let Some(t) = &c.theme {
        set("theme", "name", t, &def.theme.name);
    }
    if let Some(m) = &c.theme_mode {
        set("theme", "mode", m, def.theme.mode.as_str());
    }
    let out = doc.to_string();
    // Never write something the loader rejects.
    vk_config::Config::parse(&out, Path::new("config.toml")).map_err(|e| e.to_string())?;
    Ok(out)
}

/// Write the config atomically (temp file + rename), 0600, creating the directory.
pub fn write_config(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".config.toml.{}.tmp", std::process::id()));
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

// ---- the TUI view ------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Terminal,
    Herdr,
    Integrations,
    Notifications,
    Theme,
    Write,
}

#[derive(Debug, Clone)]
pub struct Flow {
    pub step: Step,
    pub checks: Vec<Check>,
    pub herdr: Option<PathBuf>,
    pub harnesses: Vec<HarnessRow>,
    pub sel: usize,
    /// Showing the selected harness's diff (scroll offset).
    pub diff: Option<usize>,
    /// `i` pressed: waiting for `y` to install the selected harnesses.
    pub confirm_install: bool,
    pub choices: Choices,
    pub dirs: Dirs,
    pub bin: PathBuf,
    pub config_path: PathBuf,
    pub notice: Option<String>,
    /// The written config, once done.
    pub written: Option<PathBuf>,
}

impl Flow {
    pub fn new(app: &App, dirs: Dirs, config_path: PathBuf) -> Flow {
        let bin = vibeke_bin();
        Flow {
            step: Step::Terminal,
            checks: terminal_checks(&app.caps, app.kitty, app.osc52),
            herdr: herdr_config(),
            harnesses: detect_harnesses(&dirs, &bin),
            sel: 0,
            diff: None,
            confirm_install: false,
            choices: Choices {
                notifications: Some(app.config.notifications.channel.as_str().to_string()),
                theme: Some(app.config.theme.name.clone()),
                theme_mode: Some(app.config.theme.mode.as_str().to_string()),
                herdr_toml: None,
            },
            dirs,
            bin,
            config_path,
            notice: None,
            written: None,
        }
    }

    fn steps(&self) -> Vec<Step> {
        let mut v = vec![Step::Terminal];
        if self.herdr.is_some() {
            v.push(Step::Herdr);
        }
        v.extend([
            Step::Integrations,
            Step::Notifications,
            Step::Theme,
            Step::Write,
        ]);
        v
    }

    fn go(&mut self, delta: i32) {
        let s = self.steps();
        let i = s.iter().position(|x| *x == self.step).unwrap_or(0) as i32;
        let j = (i + delta).clamp(0, s.len() as i32 - 1) as usize;
        self.step = s[j];
        self.sel = match self.step {
            Step::Notifications => ["native", "osc", "none"]
                .iter()
                .position(|n| self.choices.notifications.as_deref() == Some(*n))
                .unwrap_or(0),
            Step::Theme => THEMES
                .iter()
                .position(|n| self.choices.theme.as_deref() == Some(*n))
                .unwrap_or(0),
            _ => 0,
        };
        self.diff = None;
        self.confirm_install = false;
        self.notice = None;
    }

    /// The text step 6 writes (or the reason it can't).
    pub fn preview(&self) -> Result<String, String> {
        let existing = std::fs::read_to_string(&self.config_path).ok();
        starter_config(existing.as_deref(), &self.choices)
    }
}

/// Open at startup when `onboarding = true` or there is no config file (local clients only).
pub fn maybe_open(app: &mut App) {
    let path = vk_config::config_path();
    let local = app.machines.first().is_some_and(|m| m.local);
    if local && (app.config.onboarding || !path.exists()) {
        open_with(app, Dirs::from_env(), path);
    }
}

pub fn open_with(app: &mut App, dirs: Dirs, config_path: PathBuf) {
    app.ux.onboarding = Some(Flow::new(app, dirs, config_path));
    app.mode = Mode::Popup(Popup::Onboarding);
}

pub fn action(app: &mut App, action: &str) -> bool {
    if matches!(action, "setup" | "settings" | "onboarding") {
        open_with(app, Dirs::from_env(), vk_config::config_path());
        return true;
    }
    false
}

fn keep(app: &mut App) {
    app.mode = Mode::Popup(Popup::Onboarding);
}

pub fn key(app: &mut App, ev: KeyEvent) {
    if ev.kind == KeyKind::Release {
        return keep(app);
    }
    let Some(mut f) = app.ux.onboarding.take() else {
        return;
    };
    let k = ev.key;
    // Global navigation (not while a confirmation or a diff is up).
    if !f.confirm_install && f.diff.is_none() {
        match k {
            Key::Named(NamedKey::Escape) => {
                // Closed for this run; `:setup` reopens it.
                if f.written.is_none() {
                    app.toast("setup skipped for now — :setup any time");
                }
                return;
            }
            Key::Named(NamedKey::Tab) if !ev.mods.shift() => {
                f.go(1);
                app.ux.onboarding = Some(f);
                return keep(app);
            }
            Key::Named(NamedKey::Tab) => {
                f.go(-1);
                app.ux.onboarding = Some(f);
                return keep(app);
            }
            _ => {}
        }
    }
    match f.step {
        Step::Terminal => {
            if matches!(k, Key::Named(NamedKey::Enter) | Key::Char('n')) {
                f.go(1);
            }
        }
        Step::Herdr => match k {
            Key::Char('y' | 'Y') | Key::Named(NamedKey::Enter) => {
                if let Some(p) = &f.herdr {
                    match std::fs::read_to_string(p) {
                        Ok(src) => {
                            let rep = vk_compat::import_config(&src);
                            f.notice = Some(format!(
                                "will import {} Herdr setting(s) ({} unsupported)",
                                rep.mapped.len(),
                                rep.unsupported.len()
                            ));
                            f.choices.herdr_toml = Some(rep.toml);
                        }
                        Err(e) => f.notice = Some(format!("can't read {}: {e}", p.display())),
                    }
                }
                f.go(1);
            }
            Key::Char('n' | 'N') => {
                f.choices.herdr_toml = None;
                f.go(1);
            }
            _ => {}
        },
        Step::Integrations => integrations_key(app, &mut f, &ev),
        Step::Notifications => {
            const OPTS: [&str; 3] = ["native", "osc", "none"];
            match k {
                Key::Char('j') | Key::Named(NamedKey::Down) => f.sel = (f.sel + 1).min(2),
                Key::Char('k') | Key::Named(NamedKey::Up) => f.sel = f.sel.saturating_sub(1),
                Key::Char(c @ '1'..='3') => {
                    f.sel = (c as usize) - ('1' as usize);
                    f.choices.notifications = Some(OPTS[f.sel].into());
                }
                Key::Char(' ') => f.choices.notifications = Some(OPTS[f.sel].into()),
                Key::Char('t') => {
                    app.command_on(
                        0,
                        "notification.send",
                        json!({"title": "Vibeke test notification", "body": "Click it: Vibeke should come to the front.", "urgency": "normal"}),
                        Pending::Toast("test notification sent".into()),
                    );
                    f.notice =
                        Some("sent — did it show up (and focus Vibeke when clicked)?".into());
                }
                Key::Named(NamedKey::Enter) => {
                    f.choices.notifications = Some(OPTS[f.sel].into());
                    f.go(1);
                }
                _ => {}
            }
        }
        Step::Theme => match k {
            Key::Char('j') | Key::Named(NamedKey::Down) => {
                f.sel = (f.sel + 1).min(THEMES.len() - 1);
                app.theme = crate::theme::Theme::named(THEMES[f.sel]);
            }
            Key::Char('k') | Key::Named(NamedKey::Up) => {
                f.sel = f.sel.saturating_sub(1);
                app.theme = crate::theme::Theme::named(THEMES[f.sel]);
            }
            Key::Char('a') => {
                let auto = f.choices.theme_mode.as_deref() == Some("auto");
                f.choices.theme_mode = Some(if auto { "dark" } else { "auto" }.into());
            }
            Key::Named(NamedKey::Enter) | Key::Char(' ') => {
                f.choices.theme = Some(THEMES[f.sel].into());
                app.theme = crate::theme::Theme::named(THEMES[f.sel]);
                if k == Key::Named(NamedKey::Enter) {
                    f.go(1);
                }
            }
            _ => {}
        },
        Step::Write => match k {
            Key::Named(NamedKey::Enter) | Key::Char('w') if f.written.is_none() => {
                match f
                    .preview()
                    .and_then(|t| write_config(&f.config_path, &t).map_err(|e| e.to_string()))
                {
                    Ok(()) => {
                        app.config.onboarding = false;
                        f.notice = Some(format!(
                            "wrote {} — esc to start (:setup reopens this)",
                            f.config_path.display()
                        ));
                        f.written = Some(f.config_path.clone());
                    }
                    Err(e) => f.notice = Some(format!("not written: {e}")),
                }
            }
            Key::Named(NamedKey::Enter) => {
                // Done.
                return;
            }
            _ => {}
        },
    }
    app.ux.onboarding = Some(f);
    keep(app);
}

fn integrations_key(app: &mut App, f: &mut Flow, ev: &KeyEvent) {
    let n = f.harnesses.len();
    if f.confirm_install {
        match ev.key {
            Key::Char('y' | 'Y') => {
                let mut wrote = 0;
                for r in f.harnesses.iter_mut().filter(|r| r.selected) {
                    r.result = Some(match install(r.harness, &f.dirs, &f.bin) {
                        Ok(paths) if paths.is_empty() => "already installed".into(),
                        Ok(paths) => {
                            wrote += paths.len();
                            r.state = InstallState::Installed;
                            format!(
                                "wrote {}",
                                paths
                                    .iter()
                                    .map(|p| p.display().to_string())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            )
                        }
                        Err(e) => format!("failed: {e}"),
                    });
                    r.selected = false;
                }
                f.notice = Some(format!("{wrote} file(s) written"));
            }
            _ => f.notice = Some("nothing installed".into()),
        }
        f.confirm_install = false;
        return;
    }
    if let Some(off) = f.diff {
        match ev.key {
            Key::Char('j') | Key::Named(NamedKey::Down) => f.diff = Some(off + 1),
            Key::Char('k') | Key::Named(NamedKey::Up) => f.diff = Some(off.saturating_sub(1)),
            _ => f.diff = None,
        }
        return;
    }
    match ev.key {
        Key::Char('j') | Key::Named(NamedKey::Down) => f.sel = (f.sel + 1).min(n.saturating_sub(1)),
        Key::Char('k') | Key::Named(NamedKey::Up) => f.sel = f.sel.saturating_sub(1),
        Key::Char(' ') => {
            if let Some(r) = f.harnesses.get_mut(f.sel) {
                r.selected = !r.selected && !r.files.is_empty();
            }
        }
        Key::Char('d') => f.diff = Some(0),
        Key::Char('i') => {
            if f.harnesses.iter().any(|r| r.selected) {
                f.confirm_install = true;
            } else {
                f.notice = Some("select a harness with space first".into());
            }
        }
        Key::Named(NamedKey::Enter) | Key::Char('n') => f.go(1),
        _ => {}
    }
    let _ = app;
}

pub fn draw(app: &App, g: &mut Grid) {
    let Some(f) = &app.ux.onboarding else {
        return;
    };
    let t = app.theme;
    let steps = f.steps();
    let idx = steps.iter().position(|s| *s == f.step).unwrap_or(0);
    let mut a = crate::drafts::Area::open(
        app,
        g,
        &format!("Welcome to Vibeke · setup {}/{}", idx + 1, steps.len()),
    );
    let names: Vec<String> = steps
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let n = match s {
                Step::Terminal => "terminal",
                Step::Herdr => "herdr",
                Step::Integrations => "integrations",
                Step::Notifications => "notifications",
                Step::Theme => "theme",
                Step::Write => "write config",
            };
            if i == idx {
                format!("[{n}]")
            } else {
                n.to_string()
            }
        })
        .collect();
    a.line(&names.join(" › "), t.dim());
    a.line("", t.text());
    match f.step {
        Step::Terminal => {
            a.line("Your terminal:", t.bold(t.fg));
            for c in &f.checks {
                let (mark, st) = if c.ok {
                    ("✓ pass", t.s(t.green))
                } else {
                    ("! warn", t.s(t.yellow))
                };
                a.line(&format!("  {mark}  {:<14} {}", c.name, c.detail), st);
            }
            a.line("", t.text());
            a.line("vibeke doctor shows the full table any time.", t.dim());
        }
        Step::Herdr => {
            a.line(
                &format!(
                    "Herdr config found: {}",
                    f.herdr
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default()
                ),
                t.bold(t.fg),
            );
            a.line(
                "Import keybindings, theme, sidebar rules and worktree dir? [Y/n]",
                t.text(),
            );
            a.line(
                "A running Herdr session can be recreated later: vibeke import herdr --session",
                t.dim(),
            );
        }
        Step::Integrations => {
            a.line(
                "Agent integrations (hooks/extensions that report state and answer approvals):",
                t.bold(t.fg),
            );
            for (i, r) in f.harnesses.iter().enumerate() {
                let mark = if r.selected { "[x]" } else { "[ ]" };
                let found = r
                    .binary
                    .as_ref()
                    .map(|b| b.display().to_string())
                    .unwrap_or_else(|| "not on PATH".into());
                let line = format!(
                    "{mark} {:<9} {:<14} {}",
                    r.harness.id(),
                    r.state_text(),
                    found
                );
                let st = if i == f.sel {
                    t.sel(t.accent)
                } else {
                    t.text()
                };
                a.line(&line, st);
                if let Some(res) = &r.result {
                    a.line(&format!("      {res}"), t.dim());
                }
            }
            a.line("", t.text());
            if let Some(off) = f.diff {
                let r = &f.harnesses[f.sel.min(f.harnesses.len().saturating_sub(1))];
                a.line(&format!("── {} changes ──", r.harness.id()), t.bold(t.fg));
                let body = if r.diff.is_empty() {
                    "(nothing to change)".to_string()
                } else {
                    r.diff.clone()
                };
                for l in body.lines().skip(off) {
                    let st = if l.starts_with('+') {
                        t.s(t.green)
                    } else if l.starts_with('-') {
                        t.s(t.red)
                    } else {
                        t.dim()
                    };
                    a.line(l, st);
                }
            } else if f.confirm_install {
                let files: Vec<String> = f
                    .harnesses
                    .iter()
                    .filter(|r| r.selected)
                    .flat_map(|r| r.files.iter().map(|p| p.display().to_string()))
                    .collect();
                a.line("Install into these files?", t.bold(t.yellow));
                for p in files {
                    a.line(&format!("  {p}"), t.text());
                }
                a.line("[y] install   any other key cancels", t.bold(t.yellow));
            }
        }
        Step::Notifications => {
            a.line("Where should agent notifications go?", t.bold(t.fg));
            let native = native_notifier()
                .map(|n| format!(" ({n})"))
                .unwrap_or_else(|| " (none found: falls back to terminal)".into());
            for (i, (id, label)) in [
                ("native", format!("native OS notifications{native}")),
                (
                    "osc",
                    "terminal (OSC 9/777, routed by your terminal)".into(),
                ),
                ("none", "none (toasts and the sidebar only)".into()),
            ]
            .iter()
            .enumerate()
            {
                let chosen = f.choices.notifications.as_deref() == Some(*id);
                let st = if i == f.sel {
                    t.sel(t.accent)
                } else {
                    t.text()
                };
                a.line(
                    &format!("{} {}. {label}", if chosen { "●" } else { "○" }, i + 1),
                    st,
                );
            }
            a.line("", t.text());
            a.line("[t] send a test notification", t.dim());
        }
        Step::Theme => {
            a.line("Theme (previewed live):", t.bold(t.fg));
            for (i, name) in THEMES.iter().enumerate() {
                let chosen = f.choices.theme.as_deref() == Some(*name);
                let st = if i == f.sel {
                    t.sel(t.accent)
                } else {
                    t.text()
                };
                a.line(&format!("{} {name}", if chosen { "●" } else { "○" }), st);
            }
            let auto = f.choices.theme_mode.as_deref() == Some("auto");
            a.line("", t.text());
            a.line(
                &format!(
                    "[a] follow the terminal's light/dark: {}",
                    if auto { "on" } else { "off" }
                ),
                t.text(),
            );
        }
        Step::Write => {
            a.line(&format!("Write {}:", f.config_path.display()), t.bold(t.fg));
            match f.preview() {
                Ok(text) => {
                    for l in text.lines().take(16) {
                        a.line(&format!("  {l}"), t.text());
                    }
                }
                Err(e) => a.line(&format!("can't write: {e}"), t.s(t.red)),
            }
            a.line("", t.text());
            a.line(
                if f.written.is_some() {
                    "[enter] done"
                } else {
                    "[enter] write   (only non-default choices; existing keys and comments are kept)"
                },
                t.dim(),
            );
        }
    }
    if let Some(n) = &f.notice {
        a.line("", t.text());
        a.line(n, t.bold(t.yellow));
    }
    let keys = match f.step {
        Step::Integrations => {
            "j/k move · space select · d diff · i install selected · enter next · tab/shift+tab steps · esc later"
        }
        Step::Notifications | Step::Theme => {
            "j/k move · enter choose and continue · tab/shift+tab steps · esc later"
        }
        _ => "enter continue · tab/shift+tab steps · esc later",
    };
    a.footer(keys, t.dim());
}

#[cfg(test)]
#[path = "onboarding_tests.rs"]
mod tests;
