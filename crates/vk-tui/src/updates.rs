//! Update UI. A dedicated CLI worker owns downloads and installation; pane input is untouched.
use crate::app::{App, Incoming, Mode, Pending, Popup};
use crate::screen::Grid;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use vk_proto::input::{Key, KeyEvent, KeyKind, NamedKey};

const INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
#[derive(Default)]
pub struct State {
    pub status: Value,
    pub busy: bool,
    pub automatic: bool,
    pub confirm: bool,
    installing: bool,
    background: bool,
    check_error: Option<String>,
    next: Option<Instant>,
    worker: Option<Worker>,
    prefs: Option<PathBuf>,
    resume: Option<Value>,
}
struct Worker {
    exe: PathBuf,
    args: Vec<String>,
    inc: mpsc::UnboundedSender<Incoming>,
}
pub enum Event {
    Progress(Value),
    Finished(Result<Value, String>),
}

pub fn init(app: &mut App, args: Option<Vec<String>>, inc: mpsc::UnboundedSender<Incoming>) {
    let Some(args) = args else {
        return;
    };
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let prefs = vk_config::config_path().with_file_name("update-checks.json");
    let saved: Value = std::fs::read(&prefs)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    let automatic = saved["automatic"].as_bool().unwrap_or(true);
    let elapsed = crate::drafts::now_ms()
        .saturating_sub(saved["checked_at"].as_i64().unwrap_or(0))
        .max(0) as u64;
    let delay = INTERVAL
        .saturating_sub(Duration::from_millis(elapsed))
        .max(Duration::from_secs(10));
    let mut status = saved["status"].clone();
    // A check from an older executable cannot describe this client's current version.
    if status["current_version"] != vk_proto::VERSION {
        status = Value::Null;
    }
    if let Some(v) = status.as_object_mut() {
        v.remove("server_version");
    }
    app.ux.updates = State {
        status,
        automatic,
        next: automatic.then(|| Instant::now() + delay),
        worker: Some(Worker { exe, args, inc }),
        prefs: Some(prefs),
        resume: std::env::var("VIBEKE_UPDATE_RESUME")
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok()),
        ..Default::default()
    };
}

fn save(app: &App, checked: bool) {
    let u = &app.ux.updates;
    let Some(p) = &u.prefs else {
        return;
    };
    let old: Value = std::fs::read(p)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    let v = json!({"automatic": u.automatic, "status": u.status,
        "checked_at": if checked { crate::drafts::now_ms() } else { old["checked_at"].as_i64().unwrap_or(0) }});
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = p.with_extension(format!("{}.tmp", std::process::id()));
    if std::fs::write(&tmp, v.to_string()).is_ok() {
        let _ = std::fs::rename(tmp, p);
    }
}

fn start(app: &mut App, install: bool, background: bool) {
    let u = &mut app.ux.updates;
    if u.busy {
        return;
    }
    let Some(w) = &u.worker else {
        app.toast("Update this host from a local, writable Vibeke session");
        return;
    };
    let mut args = w.args.clone();
    args.extend(["--json".into(), "update".into()]);
    if install {
        let Some(version) = u.status["version"]
            .as_str()
            .filter(|_| u.status["state"] == "available")
        else {
            return;
        };
        args.extend(["--version".into(), version.into()]);
    } else {
        args.push("--check".into());
    }
    let exe = w.exe.clone();
    let inc = w.inc.clone();
    u.busy = true;
    u.installing = install;
    u.background = background;
    u.check_error = None;
    u.confirm = false;
    if !background {
        u.status = json!({"state": if install { "downloading" } else { "checking" }, "message": if install { "Downloading update…" } else { "Checking for updates…" }});
    }
    app.dirty = true;
    tokio::spawn(async move {
        let result = worker(exe, args, &inc).await;
        let _ = inc.send(Incoming::Update(Event::Finished(result)));
    });
}

async fn worker(
    exe: PathBuf,
    args: Vec<String>,
    inc: &mpsc::UnboundedSender<Incoming>,
) -> Result<Value, String> {
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
    let mut child = tokio::process::Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| e.to_string())?;
    let stderr = child.stderr.take().unwrap();
    let errors = tokio::spawn(async move {
        let mut b = Vec::new();
        let _ = stderr.take(8192).read_to_end(&mut b).await;
        b
    });
    let mut lines = BufReader::new(child.stdout.take().unwrap().take(1024 * 1024)).lines();
    let mut last = Value::Null;
    while let Some(s) = lines.next_line().await.map_err(|e| e.to_string())? {
        let v: Value = serde_json::from_str(&s).map_err(|_| "Invalid update worker response")?;
        last = v.clone();
        if inc.send(Incoming::Update(Event::Progress(v))).is_err() {
            return Err("Update view closed".into());
        }
    }
    let success = child.wait().await.map_err(|e| e.to_string())?.success();
    let errors = errors.await.unwrap_or_default();
    if !success {
        return Err(last["message"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| String::from_utf8_lossy(&errors).into_owned()));
    }
    if !matches!(
        last["state"].as_str(),
        Some("available" | "up_to_date" | "installed")
    ) {
        return Err("Update worker ended without a result".into());
    }
    Ok(last)
}

pub fn on_event(app: &mut App, event: Event) {
    match event {
        Event::Progress(v) => {
            if !app.ux.updates.background {
                app.ux.updates.status = v;
            }
        }
        Event::Finished(result) => {
            let u = &mut app.ux.updates;
            u.busy = false;
            u.installing = false;
            u.next = u.automatic.then(|| Instant::now() + INTERVAL);
            match result {
                Ok(v) => u.status = v,
                Err(e) if u.background => {
                    u.check_error = Some(format!("Could not check for updates: {e}"))
                }
                Err(e) => u.status = json!({"state":"error", "message":e}),
            }
            if u.status["state"] == "installed" {
                let binary = u.status["binary"].as_str().unwrap_or("");
                if !binary.is_empty() {
                    let resume = json!({"binary":binary,"machine":app.m().label,"pane":app.m().focus.pane,"workspace":app.m().focus.workspace});
                    app.nav.save();
                    app.quit = Some(format!("update:{}", resume));
                }
            }
            save(app, true);
        }
    }
    app.dirty = true;
}

/// Called after leaving raw mode. The new client receives the original invocation and focus.
pub fn relaunch(reason: &str) -> Option<String> {
    use std::os::unix::process::CommandExt;
    let v: Value = serde_json::from_str(reason.strip_prefix("update:")?).ok()?;
    let bin = v["binary"].as_str()?;
    let e = std::process::Command::new(bin)
        .args(std::env::args_os().skip(1))
        .env("VIBEKE_UPDATE_RESUME", v.to_string())
        .exec();
    Some(format!(
        "Update installed; could not reopen the TUI: {e}. Run vibeke to reconnect."
    ))
}

pub fn action(app: &mut App, action: &str) -> bool {
    if !matches!(action, "update" | "check_updates") {
        return false;
    }
    app.mode = Mode::Popup(Popup::Updates);
    if action == "check_updates" || app.ux.updates.status.is_null() {
        start(app, false, false);
    }
    true
}
pub fn key(app: &mut App, ev: &KeyEvent) {
    app.mode = Mode::Popup(Popup::Updates);
    if ev.kind != KeyKind::Press {
        return;
    }
    match ev.key {
        Key::Named(NamedKey::Escape) => {
            app.ux.updates.confirm = false;
            app.mode = Mode::Normal;
        }
        Key::Char('r') if !app.ux.updates.busy => start(app, false, false),
        Key::Char('b') => {
            app.ux.updates.automatic = !app.ux.updates.automatic;
            app.ux.updates.next = app.ux.updates.automatic.then(Instant::now);
            save(app, false);
        }
        Key::Named(NamedKey::Enter)
            if app.ux.updates.status["state"] == "available" && !app.ux.updates.busy =>
        {
            if app.ux.updates.confirm {
                start(app, true, false);
            } else {
                app.ux.updates.confirm = true;
            }
        }
        _ => {}
    }
    app.dirty = true;
}
pub fn tick(app: &mut App) {
    if let Some(v) = app.ux.updates.resume.clone()
        && let Some(mi) = app
            .machines
            .iter()
            .position(|m| m.label == v["machine"].as_str().unwrap_or(""))
        && let Some(pane) = v["pane"].as_str()
        && app.machines[mi].model.panes.iter().any(|p| p.id == pane)
    {
        app.cur = mi;
        app.command("pane.focus", json!({"pane":pane}), Pending::Ignore);
        app.ux.updates.resume = None;
    }
    let u = &app.ux.updates;
    if u.worker.is_some() && u.automatic && !u.busy && u.next.is_some_and(|t| t <= Instant::now()) {
        start(app, false, true);
    }
}
/// The worker can be changing symlinks or restarting the server. A normal TUI quit must
/// not drop it partway through; checks alone are safe to cancel.
pub fn prevent_quit(app: &mut App) -> bool {
    if !app.ux.updates.installing {
        return false;
    }
    app.toast("Wait for the update to finish before closing Vibeke");
    app.mode = Mode::Popup(Popup::Updates);
    true
}
pub fn deadlines(app: &App, d: &mut crate::deadline::Deadlines) {
    let u = &app.ux.updates;
    if u.worker.is_some()
        && !u.busy
        && u.automatic
        && let Some(t) = u.next
    {
        d.at("updates", t);
    }
}
pub fn badge(app: &App) -> Option<String> {
    let v = &app.ux.updates.status;
    match v["state"].as_str()? {
        "available" => Some(format!(
            "↑ Update · v{}",
            v["version"].as_str().unwrap_or("")
        )),
        "downloading" | "verifying" | "installing" => Some("↑ Updating…".into()),
        "error" => Some("↑ Update failed · retry".into()),
        _ => None,
    }
}
pub fn draw(app: &App, g: &mut Grid) {
    let t = app.theme;
    let u = &app.ux.updates;
    let mut a = crate::drafts::Area::open(app, g, "Vibeke · Update this host");
    a.line(
        &format!("Running TUI: v{}", vk_proto::VERSION),
        t.bold(t.fg),
    );
    if let Some(v) = u.status["installed_version"].as_str() {
        a.line(&format!("Installed CLI: v{v}"), t.text());
    }
    a.line(&format!("Local session: {}", app.nav.session), t.text());
    if let Some(v) = u.status["server_version"].as_str() {
        a.line(&format!("Running server: v{v}"), t.text());
    }
    a.line("", t.text());
    let msg = crate::plugins::sanitize(
        u.status["message"]
            .as_str()
            .unwrap_or("Press r to check for updates."),
        400,
    );
    a.line(
        &msg,
        t.s(if u.status["state"] == "error" {
            t.red
        } else {
            t.accent
        }),
    );
    if let Some(url) = u.status["release_url"].as_str() {
        a.line(&crate::plugins::sanitize(url, 180), t.text());
    }
    if let Some(error) = &u.check_error {
        a.line(&crate::plugins::sanitize(error, 400), t.text());
    }
    a.line("", t.text());
    if u.confirm {
        a.line("Install this update and reopen the TUI?", t.bold(t.yellow));
        a.line(
            "Your local session restarts. Running terminal processes stay alive.",
            t.text(),
        );
        a.line(
            "Other sessions and remote hosts keep their running versions.",
            t.text(),
        );
        a.line("Enter: install and reopen   Esc: later", t.bold(t.fg));
    } else if u.status["state"] == "available" {
        a.line("Enter: update Vibeke…   Esc: later", t.bold(t.fg));
    }
    a.line(
        &format!(
            "b: background checks {}   r: check again   Esc: close",
            if u.automatic { "on" } else { "off" }
        ),
        t.text(),
    );
    if u.worker.is_none() {
        a.line(
            "Updates require a local, writable TUI session.",
            t.s(t.yellow),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drafts::tests::{commands, fleet, named, screen};
    #[test]
    fn background_failures_preserve_available_badge_and_install_guards_quit() {
        let (mut app, _) = fleet();
        app.ux.updates.background = true;
        app.ux.updates.status = json!({"state":"available","version":"0.3.0"});
        on_event(&mut app, Event::Finished(Err("offline".into())));
        assert!(badge(&app).unwrap().contains("0.3.0"));
        assert!(app.ux.updates.check_error.is_some());
        app.ux.updates.installing = true;
        assert!(prevent_quit(&mut app));
        assert!(matches!(app.mode, Mode::Popup(Popup::Updates)));
        app.ux.updates.installing = false;
        assert!(!prevent_quit(&mut app));
    }
    #[test]
    fn checks_never_steal_focus_or_send_pane_input() {
        let (mut app, mut rxs) = fleet();
        for rx in &mut rxs {
            commands(rx);
        }
        on_event(
            &mut app,
            Event::Finished(Ok(
                json!({"state":"available", "version":"0.3.0", "current_version":vk_proto::VERSION}),
            )),
        );
        assert!(matches!(app.mode, Mode::Normal));
        assert!(badge(&app).unwrap().contains("0.3.0"));
        for rx in &mut rxs {
            assert!(commands(rx).is_empty());
        }
        action(&mut app, "update");
        app.on_key(named(NamedKey::Enter));
        assert!(app.ux.updates.confirm);
        assert!(matches!(app.mode, Mode::Popup(Popup::Updates)));
        let mut repeat = named(NamedKey::Enter);
        repeat.kind = KeyKind::Repeat;
        app.on_key(repeat);
        assert!(!app.ux.updates.busy);
        assert!(screen(&app).contains("Install this update"));
        app.on_key(named(NamedKey::Escape));
        assert!(!app.ux.updates.confirm);
        assert!(matches!(app.mode, Mode::Normal));
        for rx in &mut rxs {
            assert!(commands(rx).is_empty());
        }
    }
    #[test]
    fn failures_keep_the_tui_running_and_ready_can_relaunch_the_same_focus() {
        let (mut app, _) = fleet();
        on_event(&mut app, Event::Finished(Err("offline".into())));
        assert_eq!(badge(&app).as_deref(), Some("↑ Update failed · retry"));
        assert!(app.quit.is_none());
        on_event(
            &mut app,
            Event::Finished(Ok(
                json!({"state":"installed","version":"0.3.0","binary":"/tmp/example/vibeke"}),
            )),
        );
        let v: Value =
            serde_json::from_str(app.quit.as_ref().unwrap().strip_prefix("update:").unwrap())
                .unwrap();
        assert_eq!(v["machine"], app.m().label);
        assert_eq!(v["pane"], json!(app.m().focus.pane));
    }
}
