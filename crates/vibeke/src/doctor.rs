//! `vibeke doctor` (01 §7.6) and `vibeke update` (01 §1.2: an update is a normal server restart;
//! holders keep every pane alive).

use serde_json::{Value, json};
use std::cmp::Ordering;
use std::io::IsTerminal;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;
use vk_agents::{Dirs, Harness, InstallState};
use vk_cli::client::{self, Client};
use vk_cli::{EXIT_API, EXIT_OK, EXIT_USAGE, Global};
use vk_server::paths::{self, Paths};

// ---- report model -----------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    Pass,
    Info,
    Warn,
    Fail,
}

impl Level {
    fn word(self) -> &'static str {
        match self {
            Level::Pass => "ok",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Fail => "FAIL",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Check {
    pub section: &'static str,
    pub level: Level,
    pub message: String,
    pub hint: Option<String>,
}

#[derive(Default, Debug)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    fn add(&mut self, section: &'static str, level: Level, message: impl Into<String>) {
        self.checks.push(Check {
            section,
            level,
            message: message.into(),
            hint: None,
        });
    }

    fn add_hint(
        &mut self,
        section: &'static str,
        level: Level,
        message: impl Into<String>,
        hint: impl Into<String>,
    ) {
        self.checks.push(Check {
            section,
            level,
            message: message.into(),
            hint: Some(hint.into()),
        });
    }

    pub fn failed(&self) -> bool {
        self.checks.iter().any(|c| c.level == Level::Fail)
    }

    fn count(&self, l: Level) -> usize {
        self.checks.iter().filter(|c| c.level == l).count()
    }

    pub fn render_text(&self) -> String {
        let mut sections: Vec<&str> = Vec::new();
        for c in &self.checks {
            if !sections.contains(&c.section) {
                sections.push(c.section);
            }
        }
        let mut out = String::new();
        for s in sections {
            out.push_str(&format!("{s}\n"));
            for c in self.checks.iter().filter(|c| c.section == s) {
                out.push_str(&format!("  {:<5} {}\n", c.level.word(), c.message));
                if let Some(h) = &c.hint {
                    out.push_str(&format!("        fix: {h}\n"));
                }
            }
            out.push('\n');
        }
        out.push_str(&format!(
            "{} ok, {} warning(s), {} failure(s)\n",
            self.count(Level::Pass),
            self.count(Level::Warn),
            self.count(Level::Fail)
        ));
        out
    }

    pub fn to_json(&self) -> Value {
        json!({
            "version": vk_proto::VERSION,
            "ok": !self.failed(),
            "summary": {
                "pass": self.count(Level::Pass),
                "info": self.count(Level::Info),
                "warn": self.count(Level::Warn),
                "fail": self.count(Level::Fail),
            },
            "checks": self.checks.iter().map(|c| json!({
                "section": c.section,
                "level": match c.level {
                    Level::Pass => "pass",
                    Level::Info => "info",
                    Level::Warn => "warn",
                    Level::Fail => "fail",
                },
                "message": c.message,
                "hint": c.hint,
            })).collect::<Vec<_>>(),
        })
    }
}

// ---- pure helpers -----------------------------------------------------------------------------

/// Runtime/state directories hold sockets and transcripts: group/world access is a failure.
pub fn perm_level(mode: u32) -> Level {
    if mode & 0o077 == 0 {
        Level::Pass
    } else {
        Level::Fail
    }
}

/// `type codex` output from an interactive shell that reveals an alias/function shadowing the
/// PATH shim.
pub fn shadow_kind(type_output: &str) -> Option<&'static str> {
    let l = type_output.to_lowercase();
    if l.contains("alias") {
        Some("alias")
    } else if l.contains("function") {
        Some("function")
    } else {
        None
    }
}

/// Host-specific advice from `TERM_PROGRAM` / XTVERSION.
pub fn host_hints(term_program: &str, xtversion: Option<&str>) -> Vec<&'static str> {
    let id = format!("{term_program} {}", xtversion.unwrap_or("")).to_lowercase();
    let mut v = Vec::new();
    if id.contains("ghostty") {
        v.push(
            "Ghostty: set `clipboard-write = allow` in the Ghostty config so OSC 52 copy works (kitty keyboard and sync updates need no setup)",
        );
    }
    if id.contains("iterm") {
        v.push(
            "iTerm2: enable Settings > General > Selection > \"Applications in terminal may access clipboard\" for OSC 52 copy",
        );
        v.push(
            "iTerm2: for full key reporting enable Profiles > Keys > General > \"Report keys using CSI u\"",
        );
    }
    v
}

pub fn parse_semver(s: &str) -> Option<(u64, u64, u64, Option<String>)> {
    let s = s.trim().trim_start_matches('v');
    let s = s.split('+').next()?;
    let (core, pre) = match s.split_once('-') {
        Some((c, p)) => (c, Some(p.to_string())),
        None => (s, None),
    };
    let mut it = core.split('.');
    let maj = it.next()?.parse().ok()?;
    let min = it.next()?.parse().ok()?;
    let pat = it.next()?.parse().ok()?;
    if it.next().is_some() {
        return None;
    }
    Some((maj, min, pat, pre))
}

fn cmp_pre(a: &str, b: &str) -> Ordering {
    let mut ai = a.split('.');
    let mut bi = b.split('.');
    loop {
        match (ai.next(), bi.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                let o = match (x.parse::<u64>(), y.parse::<u64>()) {
                    (Ok(n), Ok(m)) => n.cmp(&m),
                    (Ok(_), Err(_)) => Ordering::Less,
                    (Err(_), Ok(_)) => Ordering::Greater,
                    (Err(_), Err(_)) => x.cmp(y),
                };
                if o != Ordering::Equal {
                    return o;
                }
            }
        }
    }
}

/// Semver order; a pre-release sorts before its release; unparsable sorts lowest.
pub fn cmp_semver(a: &str, b: &str) -> Ordering {
    match (parse_semver(a), parse_semver(b)) {
        (None, None) => a.cmp(b),
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (Some(x), Some(y)) => {
            (x.0, x.1, x.2)
                .cmp(&(y.0, y.1, y.2))
                .then_with(|| match (&x.3, &y.3) {
                    (None, None) => Ordering::Equal,
                    (None, Some(_)) => Ordering::Greater,
                    (Some(_), None) => Ordering::Less,
                    (Some(p), Some(q)) => cmp_pre(p, q),
                })
        }
    }
}

/// Release target of this machine: `macos-aarch64`, `linux-x86_64`, …
pub fn platform_target() -> String {
    format!(
        "{}-{}",
        if cfg!(target_os = "macos") {
            "macos"
        } else {
            "linux"
        },
        std::env::consts::ARCH
    )
}

/// Newest `<root>/<version>/vibeke-<target>` by semver.
pub fn newest_cached(root: &Path, target: &str) -> Option<(String, PathBuf)> {
    let mut best: Option<(String, PathBuf)> = None;
    for e in std::fs::read_dir(root).ok()?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if parse_semver(&name).is_none() {
            continue;
        }
        let p = e.path().join(format!("vibeke-{target}"));
        if !p.is_file() {
            continue;
        }
        if best
            .as_ref()
            .is_none_or(|(v, _)| cmp_semver(&name, v) == Ordering::Greater)
        {
            best = Some((name, p));
        }
    }
    best
}

/// `vibeke X.Y.Z` (the `--version` line) → `X.Y.Z`.
pub fn version_from_output(out: &str) -> Option<String> {
    let v = out.lines().next()?.trim().strip_prefix("vibeke ")?.trim();
    parse_semver(v).map(|_| v.to_string())
}

fn on_path(dir: &Path) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d == dir))
        .unwrap_or(false)
}

async fn cmd_output(prog: &str, args: &[&str], secs: u64, new_session: bool) -> Option<String> {
    let mut c = tokio::process::Command::new(prog);
    c.args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    if new_session {
        // SAFETY: setsid between fork and exec is async-signal-safe. Without a controlling
        // terminal an interactive shell cannot stop on tty job control.
        unsafe {
            c.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    let out = tokio::time::timeout(Duration::from_secs(secs), c.output())
        .await
        .ok()?
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

// ---- doctor sections --------------------------------------------------------------------------

const INSTALL: &str = "install";
const SOCKETS: &str = "sockets/session";
const INTEG: &str = "integrations";
const TERMINAL: &str = "terminal";
const REMOTE: &str = "remote";
const TOPOLOGY: &str = "topology";
const ISOLATION: &str = "isolation";
const TASKS: &str = "tasks";

/// Which execution isolation levels work here (13 §11, acceptance 7). Never starts anything.
fn check_isolation(r: &mut Report) {
    for (level, res) in vk_sandbox::runner::availability() {
        match res {
            Ok(d) => r.add(ISOLATION, Level::Pass, format!("{}: {d}", level.as_str())),
            Err(h) => r.add(
                ISOLATION,
                Level::Info,
                format!("{}: unavailable — {h}", level.as_str()),
            ),
        }
    }
    // Container details (13 §11): the in-box binary, the default image, Docker Sandboxes.
    let cfg = vk_server::sandbox::load_cfg();
    let home = paths::home();
    if vk_sandbox::container::detect().is_some() {
        match vk_server::sandbox::container::linux_vibeke(&cfg, &home) {
            Some(p) => r.add(
                ISOLATION,
                Level::Pass,
                format!(
                    "container: in-box vibeke {} (egress forwarder, hooks)",
                    p.display()
                ),
            ),
            None => r.add_hint(
                ISOLATION,
                Level::Info,
                "container: no static Linux vibeke for the box — network none/open only, no hooks inside",
                format!(
                    "set [isolation.container] vibeke_linux, or put vibeke-linux-{} in $VIBEKE_ARTIFACT_DIR or ~/.cache/vibeke/releases/{}/",
                    std::env::consts::ARCH,
                    vk_proto::VERSION
                ),
            ),
        }
        match &cfg.container.image {
            Some(i) => r.add(
                ISOLATION,
                Level::Pass,
                format!(
                    "container: default image {i} (code isolation: {})",
                    cfg.container.code
                ),
            ),
            None => r.add(
                ISOLATION,
                Level::Info,
                "container: no default image; tasks need --image, .vibeke/sandbox.toml or a devcontainer",
            ),
        }
    }
    if let Some(p) = vk_sandbox::container::detect_docker_sandboxes(&home) {
        r.add(
            ISOLATION,
            Level::Info,
            format!(
                "docker sandboxes plugin at {} (detected; not used as a provider yet)",
                p.display()
            ),
        );
    }
}

fn check_install(r: &mut Report) {
    let exe = std::env::current_exe().unwrap_or_default();
    r.add(
        INSTALL,
        Level::Pass,
        format!("binary {} (vibeke {})", exe.display(), vk_proto::VERSION),
    );
    let bin_dir = paths::home().join(".local/bin");
    let stable = bin_dir.join("vibeke");
    let fix = format!(
        "vibeke update --from {} (installs under ~/.local/share/vibeke and links ~/.local/bin/vibeke)",
        exe.display()
    );
    match (std::fs::canonicalize(&stable), std::fs::canonicalize(&exe)) {
        (Ok(a), Ok(b)) if a == b => r.add(
            INSTALL,
            Level::Pass,
            format!("{} points at this binary", stable.display()),
        ),
        (Ok(a), _) => r.add_hint(
            INSTALL,
            Level::Warn,
            format!(
                "{} resolves to {}, not this binary (hooks and holders use the stable path)",
                stable.display(),
                a.display()
            ),
            fix,
        ),
        (Err(_), _) => r.add_hint(
            INSTALL,
            Level::Warn,
            format!(
                "{} does not exist (hooks and holders reference this stable path)",
                stable.display()
            ),
            fix,
        ),
    }
    if on_path(&bin_dir) {
        r.add(
            INSTALL,
            Level::Pass,
            format!("{} is on PATH", bin_dir.display()),
        );
    } else {
        r.add_hint(
            INSTALL,
            Level::Warn,
            format!("{} is not on PATH", bin_dir.display()),
            "add `export PATH=\"$HOME/.local/bin:$PATH\"` to your shell profile",
        );
    }
}

fn check_dir(r: &mut Report, what: &str, p: &Path) {
    match std::fs::metadata(p) {
        Ok(m) => {
            let mode = m.mode() & 0o777;
            let lvl = perm_level(mode);
            let msg = format!("{what} {} mode {mode:04o}", p.display());
            if lvl == Level::Pass {
                r.add(SOCKETS, lvl, msg);
            } else {
                r.add_hint(
                    SOCKETS,
                    lvl,
                    format!("{msg} (must be 0700)"),
                    format!("chmod 700 {}", p.display()),
                );
            }
            // SAFETY: getuid has no preconditions.
            if m.uid() != unsafe { libc::getuid() } {
                r.add(
                    SOCKETS,
                    Level::Fail,
                    format!("{} is owned by another user", p.display()),
                );
            }
        }
        Err(_) => r.add(
            SOCKETS,
            Level::Info,
            format!("{what} {} not created yet", p.display()),
        ),
    }
}

enum ServerProbe {
    NoSocket,
    Stale(String),
    Up(Value),
    Broken(String),
}

async fn probe_server(socket: &Path) -> ServerProbe {
    if !socket.exists() {
        return ServerProbe::NoSocket;
    }
    let s = match tokio::time::timeout(Duration::from_secs(1), client::connect(socket)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return ServerProbe::Stale(format!("{e:#}")),
        Err(_) => return ServerProbe::Stale("connect timed out".into()),
    };
    let mut c = Client::new(s);
    let r = tokio::time::timeout(Duration::from_secs(3), async {
        c.hello("cli").await?;
        c.call("server.status", json!({})).await
    })
    .await;
    match r {
        Ok(Ok(v)) => ServerProbe::Up(v),
        Ok(Err(e)) => ServerProbe::Broken(e.to_string()),
        Err(_) => ServerProbe::Broken("server did not answer within 3 s".into()),
    }
}

async fn check_sockets(r: &mut Report, g: &Global) {
    let p = Paths::new(&g.session);
    r.add(SOCKETS, Level::Info, format!("session `{}`", g.session));
    check_dir(r, "runtime dir", &paths::runtime_root());
    check_dir(r, "session runtime dir", &p.runtime);
    check_dir(r, "state dir", &paths::state_root());
    check_dir(r, "session state dir", &p.state);
    let socket = client::socket_path(&g.session, g.socket.as_deref());
    if socket.as_os_str().len() > 100 {
        r.add_hint(
            SOCKETS,
            Level::Warn,
            format!(
                "socket path is {} bytes; unix sockets are limited to ~104",
                socket.as_os_str().len()
            ),
            "use a shorter VIBEKE_RUNTIME_DIR",
        );
    }
    match probe_server(&socket).await {
        ServerProbe::NoSocket => r.add(
            SOCKETS,
            Level::Info,
            format!(
                "server not running (no socket at {}); it starts on demand",
                socket.display()
            ),
        ),
        ServerProbe::Stale(e) => r.add_hint(
            SOCKETS,
            Level::Warn,
            format!("stale socket {} ({e})", socket.display()),
            format!(
                "rm {}  (or run `vibeke server start`, which replaces it)",
                socket.display()
            ),
        ),
        ServerProbe::Broken(e) => r.add_hint(
            SOCKETS,
            Level::Fail,
            format!(
                "server at {} is not responding properly: {e}",
                socket.display()
            ),
            format!(
                "vibeke server stop; if that fails: kill $(cat {})",
                p.pidfile().display()
            ),
        ),
        ServerProbe::Up(v) => {
            let n = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
            let holders = v
                .pointer("/holders/live")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            r.add(
                SOCKETS,
                Level::Pass,
                format!(
                    "server running: pid {} version {} panes {} clients {} holders {} uptime {}s",
                    n("pid"),
                    v.get("version").and_then(Value::as_str).unwrap_or("?"),
                    n("panes"),
                    n("clients"),
                    holders,
                    n("uptime_ms") / 1000
                ),
            );
            if let Some(sv) = v.get("version").and_then(Value::as_str)
                && sv != vk_proto::VERSION
            {
                r.add_hint(
                    SOCKETS,
                    Level::Warn,
                    format!(
                        "server runs {sv} but this binary is {} (restart picks up the new binary; panes survive)",
                        vk_proto::VERSION
                    ),
                    "vibeke server restart",
                );
            }
            if v.get("degraded")
                .is_some_and(|d| !d.is_null() && d != &json!(false))
            {
                r.add(
                    SOCKETS,
                    Level::Warn,
                    format!("server reports degraded: {}", v["degraded"]),
                );
            }
        }
    }
    // Pidfile whose process is gone.
    if let Ok(s) = std::fs::read_to_string(p.pidfile())
        && let Ok(pid) = s.trim().parse::<i32>()
        && !socket.exists()
        // SAFETY: signal 0 only probes for existence.
        && unsafe { libc::kill(pid, 0) } != 0
    {
        r.add_hint(
            SOCKETS,
            Level::Warn,
            format!(
                "stale pidfile {} (pid {pid} is gone)",
                p.pidfile().display()
            ),
            format!("rm {}", p.pidfile().display()),
        );
    }
    // Holder sockets.
    let mut live = 0;
    let mut orphans = Vec::new();
    if let Ok(rd) = std::fs::read_dir(p.holders()) {
        for e in rd.flatten() {
            let path = e.path();
            if path.extension().is_none_or(|x| x != "sock") {
                continue;
            }
            match tokio::time::timeout(Duration::from_millis(500), client::connect(&path)).await {
                Ok(Ok(_)) => live += 1,
                _ => orphans.push(path),
            }
        }
    }
    if live + orphans.len() > 0 {
        r.add(
            SOCKETS,
            Level::Pass,
            format!("{live} live holder socket(s) in {}", p.holders().display()),
        );
    }
    for o in orphans {
        r.add_hint(
            SOCKETS,
            Level::Warn,
            format!("orphaned holder socket {} (no live process)", o.display()),
            format!("rm {}", o.display()),
        );
    }
}

async fn check_integrations(r: &mut Report) {
    let dirs = Dirs::from_env();
    for h in [Harness::Claude, Harness::Codex] {
        let id = h.id();
        let st = vk_agents::status(h, &dirs);
        let install = format!("vibeke integration install {id}");
        match st.state {
            InstallState::Installed => r.add(
                INTEG,
                Level::Pass,
                format!("{id} hooks installed ({})", st.file.display()),
            ),
            InstallState::Partial => r.add_hint(
                INTEG,
                Level::Warn,
                format!(
                    "{id} hooks partially installed; missing: {}",
                    st.missing_events.join(", ")
                ),
                install,
            ),
            InstallState::NotInstalled => r.add_hint(
                INTEG,
                Level::Warn,
                format!("{id} hooks not installed ({})", st.file.display()),
                install,
            ),
        }
        for f in &st.foreign {
            r.add(
                INTEG,
                Level::Info,
                format!("{id}: foreign hook (Herdr, left alone): {f}"),
            );
        }
        for p in &st.problems {
            r.add(INTEG, Level::Warn, format!("{id}: {p}"));
        }
        for t in &st.todo {
            r.add(INTEG, Level::Info, format!("{id}: todo: {t}"));
        }
        let untrusted = st
            .hooks
            .iter()
            .filter(|x| x.trust == Some(vk_agents::Trust::Untrusted))
            .count();
        if untrusted > 0 {
            r.add_hint(
                INTEG,
                Level::Warn,
                format!("{untrusted} Codex hook(s) untrusted"),
                "run /hooks in Codex once and trust the Vibeke hooks",
            );
        }
        match cmd_output(id, &["--version"], 2, false).await {
            Some(v) if !v.trim().is_empty() => r.add(
                INTEG,
                Level::Pass,
                format!("{id} binary: {}", v.lines().next().unwrap_or("").trim()),
            ),
            _ => r.add(INTEG, Level::Info, format!("{id} binary not found on PATH")),
        }
    }
    let shell = std::env::var("SHELL").unwrap_or_default();
    if !shell.is_empty() {
        if let Some(out) = cmd_output(&shell, &["-ic", "type codex"], 2, true).await {
            if let Some(kind) = shadow_kind(&out) {
                r.add_hint(
                    INTEG,
                    Level::Warn,
                    format!(
                        "a shell {kind} for `codex` may shadow the Vibeke PATH shim: {}",
                        out.trim()
                    ),
                    format!(
                        "remove the {kind} (or point it at the real codex) so panes use the shim"
                    ),
                );
            } else {
                r.add(
                    INTEG,
                    Level::Pass,
                    "no shell alias/function shadows `codex`",
                );
            }
        } else {
            r.add(
                INTEG,
                Level::Info,
                "shell alias check skipped (shell did not answer within 2 s)",
            );
        }
    }
}

pub(crate) fn probe_terminal() -> Option<vk_tui::caps::ProbeResult> {
    use std::io::Write;
    use vk_tui::caps::{EnvHints, parse_replies, probe_queries};
    crossterm::terminal::enable_raw_mode().ok()?;
    let env = EnvHints::from_env();
    let mut out = std::io::stdout();
    let _ = out.write_all(&probe_queries());
    let _ = out.flush();
    let deadline = std::time::Instant::now() + Duration::from_millis(200);
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            break;
        }
        let mut pfd = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd.
        let n = unsafe { libc::poll(&mut pfd, 1, left.as_millis().max(1) as i32) };
        if n <= 0 {
            if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        let mut tmp = [0u8; 512];
        // SAFETY: reading into a local buffer of the stated length.
        let k = unsafe { libc::read(0, tmp.as_mut_ptr().cast(), tmp.len()) };
        if k <= 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..k as usize]);
        if parse_replies(&buf, &env).complete {
            break;
        }
    }
    let _ = crossterm::terminal::disable_raw_mode();
    Some(parse_replies(&buf, &env))
}

// ---- doctor terminal --------------------------------------------------------------------------

/// What `vibeke doctor terminal` probed: the capability answers (after
/// `terminal.host_overrides`) and the graphics answers.
pub struct TermProbe {
    pub caps: vk_tui::caps::ProbeResult,
    pub graphics: vk_browser::probe::GraphicsCaps,
    /// Override keys that are graphics capabilities (`kitty_graphics`, `kitty_shm`, …).
    pub graphics_overrides: Vec<(String, bool)>,
}

/// The `vibeke doctor terminal` table (03 §6.1): one pass/warn row per host feature, then the
/// settings that decide how Vibeke uses them. `probe` is `None` when stdin/stdout is not a
/// terminal (only the environment is shown).
pub fn terminal_report(
    env: &vk_tui::caps::EnvHints,
    probe: Option<&TermProbe>,
    cfg: &vk_config::Config,
) -> Report {
    use vk_browser::probe::ModeState;
    use vk_tui::caps::{Background, Notifications, Osc52};
    const T: &str = "terminal";
    const S: &str = "settings";
    let mut r = Report::default();
    let show = |s: &str| {
        if s.is_empty() {
            "(unset)".to_string()
        } else {
            s.to_string()
        }
    };
    r.add(
        T,
        Level::Info,
        format!(
            "TERM={} TERM_PROGRAM={} COLORTERM={}",
            show(&env.term),
            show(&env.term_program),
            show(&env.colorterm)
        ),
    );
    match probe {
        None => r.add_hint(
            T,
            Level::Warn,
            "not probed: stdin/stdout is not a terminal",
            "run `vibeke doctor terminal` directly in the terminal you use Vibeke in",
        ),
        Some(p) if !p.caps.complete && p.caps.da1.is_none() => r.add_hint(
            T,
            Level::Warn,
            "no answer to capability queries within 150 ms",
            "a multiplexer in between (tmux, screen) or a slow link; run it outside them",
        ),
        Some(p) => {
            let c = &p.caps;
            if let Some(x) = &c.xtversion {
                r.add(T, Level::Info, format!("identifies as {x}"));
            }
            let g = &p.graphics;
            let ov = |k: &str| {
                p.graphics_overrides
                    .iter()
                    .find(|(n, _)| n == k)
                    .map(|(_, v)| *v)
            };
            let kitty_graphics = ov("kitty_graphics").unwrap_or(g.kitty_graphics);
            let kitty_shm = ov("kitty_shm").unwrap_or(g.kitty_shm == Some(true));
            let sgr_pixels = ov("sgr_pixels").unwrap_or(matches!(
                g.sgr_pixels,
                Some(ModeState::Set | ModeState::Reset | ModeState::PermanentlySet)
            ));
            let iterm2 =
                ov("iterm2_images").unwrap_or(env.term_program == "iTerm.app" && !kitty_graphics);
            let rows: [(bool, &str, &str); 14] = [
                (
                    c.kitty_keyboard,
                    "kitty keyboard protocol (flags 31, with associated text)",
                    "use Ghostty, Kitty, WezTerm or iTerm2 with CSI u; Vibeke falls back to modifyOtherKeys / legacy keys, so AltGr text and shift+enter may be lost",
                ),
                (
                    kitty_graphics,
                    "kitty graphics (images, browser panes)",
                    "images in panes show as an [image W×H] label; iTerm2 inline images are used for browser panes where available",
                ),
                (
                    kitty_shm,
                    "kitty graphics over shared memory (t=s)",
                    "optional; images go as compressed direct transmissions",
                ),
                (
                    c.sync_update,
                    "synchronized updates (DEC 2026)",
                    "optional; redraws may flicker on slow links",
                ),
                (
                    c.truecolor,
                    "truecolor (24-bit colour)",
                    "set COLORTERM=truecolor (ssh may need SendEnv/AcceptEnv); colours are approximated",
                ),
                (
                    c.undercurl,
                    "curly and coloured underlines",
                    "optional; underlines degrade to single (host_overrides undercurl = true if it lies)",
                ),
                (
                    c.osc52 == Osc52::Allowed,
                    "OSC 52 clipboard",
                    "cannot be probed; verify with copy mode (prefix+[) and enable clipboard access in the terminal",
                ),
                (
                    c.osc8,
                    "OSC 8 hyperlinks",
                    "optional; links still open with ctrl+click (host_overrides osc8 = true to force)",
                ),
                (
                    c.focus_events,
                    "focus events (DECSET 1004)",
                    "optional; apps miss focus-in/out redraws",
                ),
                (
                    c.bracketed_paste,
                    "bracketed paste (DECSET 2004)",
                    "pastes may be read as typed keys",
                ),
                (
                    c.sgr_mouse,
                    "SGR mouse (DECSET 1006)",
                    "clicks past column 223 may be lost",
                ),
                (
                    sgr_pixels,
                    "SGR-pixels mouse (DECSET 1016)",
                    "optional; browser-pane clicks are cell-precise only",
                ),
                (
                    c.sixel,
                    "sixel graphics",
                    "optional; Vibeke draws images with kitty graphics",
                ),
                (
                    iterm2,
                    "iTerm2 inline images (OSC 1337)",
                    "optional; the fallback for browser panes without kitty graphics",
                ),
            ];
            for (ok, what, hint) in rows {
                if ok {
                    r.add(T, Level::Pass, what.to_string());
                } else {
                    r.add_hint(T, Level::Warn, format!("{what}: not detected"), hint);
                }
            }
            match g.cell_px {
                Some((w, h)) => r.add(T, Level::Pass, format!("cell size: {w}×{h} px")),
                None => r.add_hint(
                    T,
                    Level::Warn,
                    "cell size: unknown",
                    "images and browser panes assume 10×20 px cells",
                ),
            }
            let bg = match c.background {
                Background::Light => "light",
                Background::Dark => "dark",
                Background::Unknown => "unknown",
            };
            r.add(
                T,
                if c.background == Background::Unknown {
                    Level::Warn
                } else {
                    Level::Pass
                },
                format!("background: {bg} (theme auto follows it)"),
            );
            let notes = match c.notifications {
                Notifications::Osc9 => "OSC 9",
                Notifications::Osc777 => "OSC 777",
                Notifications::Osc99 => "OSC 99",
                Notifications::None => "none known",
            };
            r.add(
                T,
                Level::Info,
                format!("native notification escape: {notes}"),
            );
        }
    }
    let altgr = match cfg.keys.altgr_mode {
        vk_config::AltgrMode::Chord => "chord (ctrl+alt / alt keys with AltGr text stay chords)",
        _ => "text (AltGr keys type their character; only altgr+… bindings match them)",
    };
    r.add(S, Level::Info, format!("keys.altgr_mode: {altgr}"));
    if probe.is_some_and(|p| !p.caps.kitty_keyboard) {
        r.add(
            S,
            Level::Info,
            "AltGr text needs the kitty keyboard protocol; with legacy keys ctrl+alt and AltGr cannot be told apart",
        );
    }
    r.add(
        S,
        Level::Info,
        format!(
            "terminal.allow_passthrough: {} (DCS tmux passthrough {})",
            cfg.terminal.allow_passthrough,
            if cfg.terminal.allow_passthrough {
                "is unwrapped in new panes"
            } else {
                "is ignored"
            }
        ),
    );
    r.add(
        S,
        Level::Info,
        format!(
            "graphics: max_image_bytes {}, max_total_per_pane {} (new panes)",
            cfg.graphics.max_image_bytes, cfg.graphics.max_total_per_pane
        ),
    );
    if !cfg.terminal.host_overrides.is_empty() {
        let o: Vec<String> = cfg
            .terminal
            .host_overrides
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        r.add(
            S,
            Level::Info,
            format!("terminal.host_overrides applied: {}", o.join(", ")),
        );
    }
    r
}

/// `vibeke doctor terminal [--json]`: probe the host terminal and print the table.
pub fn doctor_terminal(g: &Global, args: &[String]) -> i32 {
    if let Some(bad) = args.first() {
        eprintln!("vibeke doctor terminal [--json]  (unexpected `{bad}`)");
        return EXIT_USAGE;
    }
    let cfg = vk_config::Config::load(vk_config::config_path())
        .map(|(c, _)| c)
        .unwrap_or_default();
    let env = vk_tui::caps::EnvHints::from_env();
    let tty = std::io::stdout().is_terminal() && std::io::stdin().is_terminal();
    let probe = if tty && crossterm::terminal::enable_raw_mode().is_ok() {
        let (mut caps, graphics) = vk_tui::term::probe();
        let _ = crossterm::terminal::disable_raw_mode();
        let graphics_overrides = caps.apply_overrides(&cfg.terminal.host_overrides);
        Some(TermProbe {
            caps,
            graphics,
            graphics_overrides,
        })
    } else {
        None
    };
    let r = terminal_report(&env, probe.as_ref(), &cfg);
    if g.json == Some(true) {
        println!(
            "{}",
            serde_json::to_string_pretty(&r.to_json()).unwrap_or_default()
        );
    } else {
        print!("{}", r.render_text());
    }
    if r.failed() { 1 } else { EXIT_OK }
}

fn check_terminal(r: &mut Report) {
    use vk_tui::caps::{Background, EnvHints, Osc52};
    let env = EnvHints::from_env();
    let show = |s: &str| {
        if s.is_empty() {
            "(unset)".to_string()
        } else {
            s.to_string()
        }
    };
    r.add(
        TERMINAL,
        Level::Info,
        format!(
            "TERM={} TERM_PROGRAM={} COLORTERM={}",
            show(&env.term),
            show(&env.term_program),
            show(&env.colorterm)
        ),
    );
    let tty = std::io::stdout().is_terminal() && std::io::stdin().is_terminal();
    let mut xt: Option<String> = None;
    if tty {
        match probe_terminal() {
            Some(p) if p.complete || p.da1.is_some() => {
                xt = p.xtversion.clone();
                if let Some(x) = &p.xtversion {
                    r.add(TERMINAL, Level::Info, format!("terminal identifies as {x}"));
                }
                let yn = |b: bool, what: &str, hint: &str, r: &mut Report| {
                    if b {
                        r.add(TERMINAL, Level::Pass, format!("{what}: supported"));
                    } else {
                        r.add_hint(TERMINAL, Level::Warn, format!("{what}: not detected"), hint);
                    }
                };
                yn(
                    p.kitty_keyboard,
                    "kitty keyboard protocol",
                    "use Ghostty/iTerm2 (enable CSI u reporting in iTerm2); Vibeke falls back to legacy keys",
                    r,
                );
                yn(
                    p.sync_update,
                    "synchronized updates (DEC 2026)",
                    "optional; without it redraws may flicker on slow links",
                    r,
                );
                yn(
                    p.truecolor,
                    "truecolor",
                    "set COLORTERM=truecolor (ssh may need AcceptEnv/SendEnv)",
                    r,
                );
                yn(
                    p.undercurl,
                    "curly/coloured underlines",
                    "optional; underlines degrade to single (terminal.host_overrides undercurl = true if it lies)",
                    r,
                );
                yn(
                    p.osc8,
                    "OSC 8 hyperlinks",
                    "optional; links still open with ctrl+click (terminal.host_overrides osc8 = true to force)",
                    r,
                );
                yn(
                    p.focus_events,
                    "focus events (DECRQM 1004)",
                    "optional; apps miss focus-in/out redraws",
                    r,
                );
                yn(
                    p.bracketed_paste,
                    "bracketed paste (DECRQM 2004)",
                    "pastes may be read as typed keys",
                    r,
                );
                yn(
                    p.sgr_mouse,
                    "SGR mouse (DECRQM 1006)",
                    "clicks past column 223 may be lost",
                    r,
                );
                if p.sixel {
                    r.add(TERMINAL, Level::Info, "sixel graphics: reported (DA1 4)");
                }
                let notes = match p.notifications {
                    vk_tui::caps::Notifications::Osc9 => "OSC 9",
                    vk_tui::caps::Notifications::Osc777 => "OSC 777",
                    vk_tui::caps::Notifications::Osc99 => "OSC 99",
                    vk_tui::caps::Notifications::None => "none known",
                };
                r.add(
                    TERMINAL,
                    Level::Info,
                    format!("native notifications: {notes}"),
                );
                match p.osc52 {
                    Osc52::Allowed => {
                        r.add(TERMINAL, Level::Pass, "OSC 52 clipboard: assumed allowed")
                    }
                    Osc52::Unknown => r.add(
                        TERMINAL,
                        Level::Info,
                        "OSC 52 clipboard: cannot be probed; assumed on, verify with copy mode (prefix+[)",
                    ),
                }
                let bg = match p.background {
                    Background::Light => "light",
                    Background::Dark => "dark",
                    Background::Unknown => "unknown",
                };
                r.add(TERMINAL, Level::Info, format!("background: {bg}"));
            }
            Some(_) => r.add(
                TERMINAL,
                Level::Warn,
                "terminal did not answer capability queries within 200 ms (multiplexer or slow link?)",
            ),
            None => r.add(
                TERMINAL,
                Level::Info,
                "capability probe skipped (raw mode unavailable)",
            ),
        }
    } else {
        r.add(
            TERMINAL,
            Level::Info,
            "live capability probe skipped (stdout is not a TTY)",
        );
        let ct = env.colorterm.to_lowercase();
        if ct == "truecolor" || ct == "24bit" {
            r.add(TERMINAL, Level::Pass, "truecolor advertised via COLORTERM");
        } else {
            r.add(
                TERMINAL,
                Level::Info,
                "COLORTERM does not advertise truecolor",
            );
        }
    }
    for h in host_hints(&env.term_program, xt.as_deref()) {
        r.add(TERMINAL, Level::Info, h);
    }
}

async fn check_remote(r: &mut Report) {
    let cfg = crate::commands::load_config();
    if cfg.remote.machine.is_empty() {
        r.add(
            REMOTE,
            Level::Info,
            "no [[remote.machine]] configured (vibeke machine add <label> <host>)",
        );
        return;
    }
    for m in &cfg.remote.machine {
        let target = vk_remote::Target::parse(&m.label, &m.address);
        let probe = tokio::time::timeout(
            Duration::from_secs(10),
            vk_remote::bootstrap::probe(&target),
        )
        .await;
        let p = match probe {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => {
                r.add_hint(
                    REMOTE,
                    Level::Fail,
                    format!("{} ({}): probe failed: {e:#}", m.label, m.address),
                    format!("ssh {} true  (check keys, host, VPN)", m.address),
                );
                continue;
            }
            Err(_) => {
                r.add_hint(
                    REMOTE,
                    Level::Fail,
                    format!("{} ({}): probe timed out after 10 s", m.label, m.address),
                    format!(
                        "ssh {} true  (check keys, host, VPN); skip with --no-remote",
                        m.address
                    ),
                );
                continue;
            }
        };
        let t = p.target();
        let head = format!(
            "{} ({}): {} {} {}",
            m.label, m.address, p.os, p.arch, p.libc
        );
        match &p.version {
            Some(v) if v == vk_proto::VERSION => r.add(
                REMOTE,
                Level::Pass,
                format!("{head}; vibeke {v} matches local"),
            ),
            Some(v) => r.add_hint(
                REMOTE,
                Level::Warn,
                format!("{head}; vibeke {v}, local is {}", vk_proto::VERSION),
                format!("vibeke ssh {} --upgrade", m.label),
            ),
            None => r.add_hint(
                REMOTE,
                Level::Warn,
                format!("{head}; vibeke not installed"),
                format!("vibeke ssh {}", m.label),
            ),
        }
        match crate::remote::artifact_for(&t) {
            Some(a) => r.add(
                REMOTE,
                Level::Pass,
                format!("{}: local artifact for {t}: {}", m.label, a.path.display()),
            ),
            None => r.add_hint(
                REMOTE,
                Level::Warn,
                format!("{}: no local release artifact for {t}", m.label),
                "mise run dist",
            ),
        }
    }
}

/// Port pool findings (05 §6): one line per problem, so exhaustion and range conflicts show up
/// as warnings instead of as tasks silently starting without ports.
fn port_findings(r: &mut Report, h: &vk_tasks::PoolHealth) {
    r.add(
        TASKS,
        Level::Pass,
        format!(
            "port pool: {} of {} blocks free, {} leased",
            h.free, h.capacity, h.leased
        ),
    );
    for w in &h.warnings {
        r.add_hint(
            TASKS,
            Level::Warn,
            w.clone(),
            "adjust `tasks.port_pool` / `tasks.port_block` in config.toml, or finish tasks you no longer need",
        );
    }
}

fn check_tasks(r: &mut Report) {
    match vk_server::task_workspace::port_health() {
        Ok(h) => port_findings(r, &h),
        Err(e) => r.add(TASKS, Level::Info, format!("port leases unreadable: {e}")),
    }
}

fn check_topology(r: &mut Report) {
    let over_ssh =
        std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some();
    if over_ssh {
        r.add_hint(
            TOPOLOGY,
            Level::Warn,
            "running inside a plain `ssh host` session: drop/paste translation and clipboard image paste cannot reach the remote",
            "from your laptop run `vibeke ssh <host>` instead (the local client translates drops and pastes)",
        );
    } else {
        r.add(TOPOLOGY, Level::Pass, "local session (not over ssh)");
    }
    if std::env::var("VIBEKE").as_deref() == Ok("1") {
        r.add(
            TOPOLOGY,
            Level::Info,
            format!(
                "inside a Vibeke pane (session {})",
                std::env::var("VIBEKE_SESSION").unwrap_or_default()
            ),
        );
    }
}

/// `VIBEKE_TEST_HOOKS=1` + `VIBEKE_TEST_REBUILD_PAUSE=<file>`: with the state lock held, create
/// `<file>.paused` and wait (up to 60 s) until `<file>` exists (tests start a server meanwhile).
fn test_pause_rebuild() {
    if std::env::var("VIBEKE_TEST_HOOKS").as_deref() != Ok("1") {
        return;
    }
    let Some(gate) = std::env::var_os("VIBEKE_TEST_REBUILD_PAUSE").map(PathBuf::from) else {
        return;
    };
    let mut paused = gate.clone().into_os_string();
    paused.push(".paused");
    let _ = std::fs::write(&paused, b"");
    let t0 = std::time::Instant::now();
    while !gate.exists() && t0.elapsed() < Duration::from_secs(60) {
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// `vibeke doctor --rebuild-index` (02 "Archive search as implemented", 07 §7): rebuild
/// `scrollback_fts` and `archive_panes` from the zstd segments on disk. Works offline on the
/// session's state dir and refuses while the session's server is running (it would be writing
/// the same tables); segment files are never modified.
/// Whether the session's server is running (its socket answers or its pidfile's process lives).
pub(crate) async fn session_running(g: &Global) -> bool {
    let p = Paths::new(&g.session);
    let socket = client::socket_path(&g.session, g.socket.as_deref());
    let pid_alive = std::fs::read_to_string(p.pidfile())
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .is_some_and(|pid| pid > 0 && unsafe { libc::kill(pid, 0) } == 0);
    !matches!(
        probe_server(&socket).await,
        ServerProbe::NoSocket | ServerProbe::Stale(_)
    ) || pid_alive
}

async fn rebuild_index(g: &Global) -> i32 {
    let p = Paths::new(&g.session);
    let running = session_running(g).await;
    if running {
        eprintln!(
            "refusing to rebuild the index of session `{}` while its server is running (it writes the same tables); run `vibeke server stop` first (panes survive), then retry",
            g.session
        );
        return 1;
    }
    if !p.db().exists() {
        eprintln!(
            "no state database at {} (session `{}`): nothing to rebuild",
            p.db().display(),
            g.session
        );
        return 1;
    }
    // The probe above is a snapshot: a server could start right after it. The state dir's
    // writer lock, which a server holds for its whole life, closes that window — taken here
    // and held until the rebuild is done, a server that starts meanwhile waits and then
    // refuses (leftovers review finding 12).
    let _lock = match p.try_lock_state() {
        Ok(Some(l)) => l,
        Ok(None) => {
            eprintln!(
                "refusing to rebuild the index of session `{}`: its state is locked ({}), so its server is running (or starting) or another rebuild is in progress; run `vibeke server stop` first (panes survive), then retry",
                g.session,
                p.state_lock().display()
            );
            return 1;
        }
        Err(e) => {
            eprintln!("cannot lock {}: {e}", p.state_lock().display());
            return 1;
        }
    };
    test_pause_rebuild();
    let report = match vk_store::Store::open(&p.db()).and_then(|s| {
        // Settle purges a crash interrupted first: their staged segments belong back in (or
        // out of) the archive before it is re-indexed.
        s.recover_archive_purges(&p.scrollback())?;
        s.rebuild_archive_index(&p.scrollback())
    }) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("rebuild failed (the old index is unchanged): {e:#}");
            return 1;
        }
    };
    if g.json == Some(true) || !std::io::stdout().is_terminal() {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"session": g.session, "rebuilt": report}))
                .unwrap_or_default()
        );
    } else {
        println!(
            "session `{}`: re-indexed {} rows from {} segments of {} panes (was {} rows); archive_panes {} -> {}",
            g.session,
            report.rows_indexed,
            report.segments,
            report.panes,
            report.fts_rows_before,
            report.archive_panes_before,
            report.archive_panes_after
        );
        for d in &report.damaged {
            println!("  damaged, indexed up to the damage: {d}");
        }
        for k in &report.skipped {
            println!("  skipped (nothing readable): {k}");
        }
        println!(
            "segment files were not modified; timestamps of re-indexed rows are the segment files' modification times"
        );
    }
    EXIT_OK
}

const AUDIT: &str = "audit";

/// The session's audit log hash chain (09 §11): intact, cut or edited. `all` lists every
/// problem; otherwise the first few.
fn check_audit(r: &mut Report, g: &Global, all: bool) {
    let log = Paths::new(&g.session).audit_log();
    let v = vk_server::audit::verify_file(&log);
    if !v.exists && v.ok() {
        r.add(
            AUDIT,
            Level::Info,
            format!("no audit log yet ({})", log.display()),
        );
        return;
    }
    if v.ok() {
        r.add(
            AUDIT,
            Level::Pass,
            format!(
                "hash chain intact: {} entries ({})",
                v.entries,
                log.display()
            ),
        );
    } else {
        let shown = if all { v.problems.len() } else { 3 };
        for p in v.problems.iter().take(shown) {
            r.add_hint(
                AUDIT,
                Level::Fail,
                p.clone(),
                "the audit log was edited or truncated outside Vibeke; keep a copy for inspection",
            );
        }
        if v.problems.len() > shown {
            r.add(
                AUDIT,
                Level::Fail,
                format!(
                    "{} more problems (vibeke doctor --audit)",
                    v.problems.len() - shown
                ),
            );
        }
    }
    for seq in &v.discontinuities {
        r.add(
            AUDIT,
            Level::Warn,
            format!("entry {seq}: a server found the log cut short and continued it"),
        );
    }
}

pub async fn run(g: &Global, args: &[String]) -> i32 {
    if args.first().map(String::as_str) == Some("terminal") {
        return doctor_terminal(g, &args[1..]);
    }
    if args
        .iter()
        .any(|a| a == "--list-backups" || a == "--restore-backup")
    {
        return crate::state_backup::run(g, args).await;
    }
    if args.iter().any(|a| a == "--rebuild-index") {
        if let Some(bad) = args.iter().find(|a| a.as_str() != "--rebuild-index") {
            eprintln!("vibeke doctor --rebuild-index takes no other flag  (unexpected `{bad}`)");
            return EXIT_USAGE;
        }
        return rebuild_index(g).await;
    }
    if let Some(bad) = args
        .iter()
        .find(|a| !matches!(a.as_str(), "--no-remote" | "--audit"))
    {
        eprintln!(
            "vibeke doctor [--json] [--no-remote] [--audit] | --rebuild-index | --list-backups | --restore-backup NAME | terminal  (unexpected `{bad}`)"
        );
        return EXIT_USAGE;
    }
    if args.iter().any(|a| a == "--audit") {
        // Only the audit log (09 §11), with every problem listed.
        let mut r = Report::default();
        check_audit(&mut r, g, true);
        if g.json == Some(true) {
            println!(
                "{}",
                serde_json::to_string_pretty(&r.to_json()).unwrap_or_default()
            );
        } else {
            print!("{}", r.render_text());
        }
        return if r.failed() { 1 } else { EXIT_OK };
    }
    let no_remote = args.iter().any(|a| a == "--no-remote");
    let mut r = Report::default();
    check_audit(&mut r, g, false);
    check_install(&mut r);
    check_sockets(&mut r, g).await;
    check_integrations(&mut r).await;
    check_terminal(&mut r);
    if no_remote {
        r.add(REMOTE, Level::Info, "skipped (--no-remote)");
    } else {
        check_remote(&mut r).await;
    }
    check_topology(&mut r);
    check_isolation(&mut r);
    check_tasks(&mut r);
    if g.json == Some(true) {
        println!(
            "{}",
            serde_json::to_string_pretty(&r.to_json()).unwrap_or_default()
        );
    } else {
        print!("{}", r.render_text());
    }
    if r.failed() { 1 } else { EXIT_OK }
}

// ---- update -----------------------------------------------------------------------------------

/// Local install layout, mirroring the remote bootstrap (`vk_remote::bootstrap`).
pub struct Layout {
    pub data: PathBuf,
    pub bin: PathBuf,
}

impl Layout {
    pub fn from_env() -> Self {
        Layout {
            data: paths::data_root(),
            bin: paths::home().join(".local/bin"),
        }
    }
    fn version_bin(&self, v: &str) -> PathBuf {
        self.data.join("versions").join(v).join("vibeke")
    }
    fn current(&self) -> PathBuf {
        self.data.join("current")
    }
    pub fn current_version(&self) -> Option<String> {
        let t = std::fs::read_link(self.current()).ok()?;
        Some(t.file_name()?.to_string_lossy().into_owned())
    }
    pub fn previous_version(&self) -> Option<String> {
        let s = std::fs::read_to_string(self.data.join("previous")).ok()?;
        Some(s.trim().to_string()).filter(|s| !s.is_empty())
    }
}

fn atomic_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    let mut t = link.as_os_str().to_owned();
    t.push(".tmp");
    let tmp = PathBuf::from(t);
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(target, &tmp)?;
    std::fs::rename(&tmp, link)
}

/// Copy `src` to `versions/<v>/vibeke` (tmp file + rename).
pub fn install_version(l: &Layout, v: &str, src: &Path) -> std::io::Result<PathBuf> {
    let dest = l.version_bin(v);
    let dir = dest.parent().expect("version dir");
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(&l.data, std::fs::Permissions::from_mode(0o700))?;
    let tmp = dir.join("vibeke.tmp");
    std::fs::copy(src, &tmp)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&tmp, &dest)?;
    Ok(dest)
}

/// Point `current` at `versions/<v>` atomically, remember the previous version, link
/// `~/.local/bin/vibeke`, and prune everything but current + previous.
pub fn switch_current(l: &Layout, v: &str) -> std::io::Result<Option<String>> {
    let prev = l.current_version();
    atomic_symlink(&PathBuf::from("versions").join(v), &l.current())?;
    if let Some(p) = prev.as_ref().filter(|p| p.as_str() != v) {
        std::fs::write(l.data.join("previous"), format!("{p}\n"))?;
    }
    std::fs::create_dir_all(&l.bin)?;
    let link = l.bin.join("vibeke");
    if let Ok(m) = std::fs::symlink_metadata(&link)
        && !m.file_type().is_symlink()
    {
        // A regular file we did not create: keep it.
        std::fs::rename(&link, l.bin.join("vibeke.old"))?;
    }
    atomic_symlink(&l.current().join("vibeke"), &link)?;
    let keep: Vec<String> = [Some(v.to_string()), l.previous_version()]
        .into_iter()
        .flatten()
        .collect();
    if let Ok(rd) = std::fs::read_dir(l.data.join("versions")) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if !keep.contains(&n) {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
    Ok(prev)
}

#[cfg(test)]
fn sidecar(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".sha256");
    PathBuf::from(s)
}

async fn binary_version(p: &Path) -> Option<String> {
    cmd_output(&p.to_string_lossy(), &["--version"], 5, false)
        .await
        .and_then(|o| version_from_output(&o))
}

fn releases_root() -> PathBuf {
    paths::home().join(".cache/vibeke/releases")
}

/// `--from` may be a binary or a directory containing `vibeke-<target>`.
fn resolve_from(p: &Path) -> PathBuf {
    if p.is_dir() {
        p.join(format!("vibeke-{}", platform_target()))
    } else {
        p.to_path_buf()
    }
}

async fn pane_counts(g: &Global) -> Option<(u64, u64)> {
    let socket = client::socket_path(&g.session, g.socket.as_deref());
    match probe_server(&socket).await {
        ServerProbe::Up(v) => Some((
            v.get("panes").and_then(Value::as_u64).unwrap_or(0),
            v.pointer("/holders/live")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        )),
        _ => None,
    }
}

fn spawn_server_with(bin: &Path, session: &str) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;
    let paths = Paths::new(session);
    paths.ensure()?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.logs().join("server.log"))?;
    let mut cmd = std::process::Command::new(bin);
    cmd.args(["server", "--session", session])
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    for k in vk_server::run::PANE_IDENTITY_ENV {
        cmd.env_remove(k);
    }
    // SAFETY: setsid between fork and exec is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn()?;
    Ok(())
}

type Counts = Option<(u64, u64)>;

/// Stop the session server over the API (holders keep the panes) and start `bin` in its place.
/// Returns the pane/holder counts seen before and after.
async fn restart_server(g: &Global, bin: &Path) -> Result<(Counts, Counts), String> {
    let socket = client::socket_path(&g.session, g.socket.as_deref());
    let before = pane_counts(g).await;
    if before.is_none() {
        return Ok((None, None));
    }
    let s = client::connect(&socket)
        .await
        .map_err(|e| format!("{e:#}"))?;
    let mut c = Client::new(s);
    c.hello("cli").await.map_err(|e| e.to_string())?;
    match tokio::time::timeout(Duration::from_secs(10), c.call("server.stop", json!({}))).await {
        Ok(Ok(_)) | Ok(Err(client::CallError::Io(_))) => {}
        Ok(Err(e)) => return Err(format!("server.stop: {e}")),
        Err(_) => return Err("server.stop timed out".into()),
    }
    drop(c);
    let mut gone = false;
    for _ in 0..100 {
        if tokio::net::UnixStream::connect(&socket).await.is_err() {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    if !gone {
        return Err("old server did not exit within 5 s".into());
    }
    spawn_server_with(bin, &g.session).map_err(|e| format!("spawn server: {e:#}"))?;
    let mut after = None;
    for _ in 0..250 {
        if let Some(a) = pane_counts(g).await {
            after = Some(a);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    if after.is_none() {
        return Err(format!(
            "new server did not come up within 5 s (see {})",
            Paths::new(&g.session).logs().join("server.log").display()
        ));
    }
    Ok((before, after))
}

fn report_restart(before: Counts, after: Counts) -> i32 {
    match (before, after) {
        (None, _) => {
            println!("no server running; the next `vibeke` starts the new version");
            EXIT_OK
        }
        (Some(b), Some(a)) => {
            println!("before: {} pane(s), {} holder(s)", b.0, b.1);
            println!("after:  {} pane(s), {} holder(s)", a.0, a.1);
            if a.0 >= b.0 {
                println!("server restarted; no panes lost");
                EXIT_OK
            } else {
                eprintln!("warning: pane count dropped from {} to {}", b.0, a.0);
                EXIT_API
            }
        }
        (Some(_), None) => EXIT_API,
    }
}

fn restart_bin(l: &Layout) -> PathBuf {
    let stable = l.bin.join("vibeke");
    if stable.exists() {
        stable
    } else {
        l.version_bin(&l.current_version().unwrap_or_default())
    }
}

pub async fn update(g: &Global, args: &[String]) -> i32 {
    let mut check = false;
    let mut rollback = false;
    let mut force = false;
    let mut allow_downgrade = false;
    let mut from: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--check" => check = true,
            "--rollback" => rollback = true,
            "--force" => force = true,
            "--allow-downgrade" => allow_downgrade = true,
            "--from" => {
                i += 1;
                match args.get(i) {
                    Some(p) => from = Some(PathBuf::from(p)),
                    None => {
                        eprintln!("--from needs a path");
                        return EXIT_USAGE;
                    }
                }
            }
            other => {
                eprintln!(
                    "vibeke update [--check] [--from <path>] [--rollback] [--force] [--allow-downgrade]  (unexpected `{other}`)"
                );
                return EXIT_USAGE;
            }
        }
        i += 1;
    }
    if g.machine.is_some() {
        eprintln!(
            "vibeke update only updates this machine; use `vibeke ssh <host> --upgrade` for remotes"
        );
        return EXIT_USAGE;
    }
    let layout = Layout::from_env();

    if rollback {
        let Some(prev) = layout.previous_version() else {
            eprintln!("no previous version recorded; nothing to roll back to");
            return EXIT_API;
        };
        if !layout.version_bin(&prev).is_file() {
            eprintln!("previous version {prev} is no longer installed");
            return EXIT_API;
        }
        if let Err(e) = switch_current(&layout, &prev) {
            eprintln!("switch to {prev}: {e}");
            return EXIT_API;
        }
        println!("current -> {prev}");
        return finish_restart(g, &layout).await;
    }

    // Pick the candidate: --from, else the newest cached artifact for this platform.
    let target = platform_target();
    let (cand_path, cand_version, signed) = match &from {
        Some(p) => {
            let p = resolve_from(p);
            if !p.is_file() {
                eprintln!("{} is not a file", p.display());
                return EXIT_USAGE;
            }
            // Nothing from the candidate is executed until its checksum (and signature or
            // the explicit opt-in) has been verified.
            let signed = match verify_candidate(&p) {
                Ok(s) => s,
                Err(code) => return code,
            };
            match binary_version(&p).await {
                Some(v) => (p, v, signed),
                None => {
                    eprintln!(
                        "{} does not look like a vibeke binary (`--version` failed)",
                        p.display()
                    );
                    return EXIT_USAGE;
                }
            }
        }
        // The version of a cached artifact is its directory name; nothing is executed.
        None => match newest_cached(&releases_root(), &target) {
            Some((v, p)) => {
                let signed = match verify_candidate(&p) {
                    Ok(s) => s,
                    Err(code) => return code,
                };
                (p, v, signed)
            }
            None => {
                println!("vibeke {}", vk_proto::VERSION);
                println!(
                    "no cached artifact for {target} in {} (build with `mise run dist`, or pass --from)",
                    releases_root().display()
                );
                return if check { EXIT_OK } else { EXIT_API };
            }
        },
    };
    // A signed candidate must be signed for the version it is (09 §10): an older release's
    // valid signature can't be replayed as a newer one.
    if let Err(e) =
        vk_remote::bootstrap::require_signed_version(signed.0, signed.1.as_deref(), &cand_version)
    {
        eprintln!("{e:#}");
        return EXIT_API;
    }
    let ord = cmp_semver(&cand_version, vk_proto::VERSION);
    if check {
        println!("current: vibeke {}", vk_proto::VERSION);
        match ord {
            Ordering::Greater => println!(
                "newer version available: {cand_version} ({}); run `vibeke update{}`",
                cand_path.display(),
                from.as_ref()
                    .map(|p| format!(" --from {}", p.display()))
                    .unwrap_or_default()
            ),
            _ => println!(
                "up to date (found {cand_version} at {})",
                cand_path.display()
            ),
        }
        return EXIT_OK;
    }
    if from.is_none() && ord != Ordering::Greater && !force {
        println!(
            "vibeke {} is up to date (newest cached: {cand_version}); use --force to reinstall",
            vk_proto::VERSION
        );
        return EXIT_OK;
    }
    if let Err(msg) = downgrade_allowed(&cand_version, vk_proto::VERSION, allow_downgrade) {
        eprintln!("{msg}");
        return EXIT_API;
    }
    if let Err(e) = install_version(&layout, &cand_version, &cand_path) {
        eprintln!("install {cand_version}: {e}");
        return EXIT_API;
    }
    match switch_current(&layout, &cand_version) {
        Ok(prev) => println!(
            "installed {cand_version}: current -> versions/{cand_version} (previous: {})",
            prev.filter(|p| *p != cand_version)
                .unwrap_or_else(|| "none".into())
        ),
        Err(e) => {
            eprintln!("switch current: {e}");
            return EXIT_API;
        }
    }
    finish_restart(g, &layout).await
}

/// `vibeke update` never installs an older version than the running one unless asked
/// (`--allow-downgrade`; `--rollback` is the explicit way back to the previous version).
fn downgrade_allowed(candidate: &str, current: &str, allow: bool) -> Result<(), String> {
    if cmp_semver(candidate, current) == Ordering::Less && !allow {
        return Err(format!(
            "refusing to downgrade from {current} to {candidate}; pass --allow-downgrade to do it on purpose (or `vibeke update --rollback`)"
        ));
    }
    Ok(())
}

/// Check the candidate's checksum and signature/opt-in before anything else touches it.
/// Returns how it is trusted and, when signed, the verified trusted comment.
fn verify_candidate(p: &Path) -> Result<(vk_remote::bootstrap::Trust, Option<String>), i32> {
    let allow = vk_remote::bootstrap::allow_unsigned_env();
    match vk_remote::bootstrap::trust_artifact_signed(p, allow) {
        Ok((sha, trust, comment)) => {
            if trust == vk_remote::bootstrap::Trust::Signed {
                println!("sha256 {sha} verified; SHA256SUMS signature verified");
            } else {
                eprintln!("{}", vk_remote::bootstrap::unsigned_warning(p, &sha));
            }
            Ok((trust, comment))
        }
        Err(e) => {
            eprintln!("{e:#}");
            Err(EXIT_API)
        }
    }
}

async fn finish_restart(g: &Global, layout: &Layout) -> i32 {
    match restart_server(g, &restart_bin(layout)).await {
        Ok((b, a)) => report_restart(b, a),
        Err(e) => {
            eprintln!(
                "restart failed: {e}\nthe new binary is installed; retry with `vibeke server restart`"
            );
            EXIT_API
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn port_pool_problems_are_warnings_and_a_healthy_pool_is_not() {
        let pool = vk_tasks::PortPool::parse("20000-20029", 10).unwrap();
        let lease = |s: u16, id: &str| vk_tasks::Lease {
            start: s,
            end: s + 9,
            task_id: id.into(),
            session: "s".into(),
            owner_pid: None,
            created_at: 0,
        };
        let healthy = vk_tasks::pool_health(pool, &[], Some((32768, 60999)));
        let mut r = Report::default();
        port_findings(&mut r, &healthy);
        assert_eq!(r.checks.len(), 1);
        assert_eq!(r.checks[0].level, Level::Pass);
        assert!(r.checks[0].message.contains("3 of 3 blocks free"));

        let full = [lease(20000, "a"), lease(20010, "b"), lease(20020, "c")];
        let exhausted = vk_tasks::pool_health(pool, &full, Some((20015, 60999)));
        let mut r = Report::default();
        port_findings(&mut r, &exhausted);
        let warns: Vec<_> = r
            .checks
            .iter()
            .filter(|c| c.level == Level::Warn)
            .map(|c| c.message.as_str())
            .collect();
        assert!(warns.iter().any(|w| w.contains("exhausted")), "{warns:?}");
        assert!(warns.iter().any(|w| w.contains("ephemeral")), "{warns:?}");
        assert!(!r.failed(), "warnings never fail the doctor");
    }

    use super::*;

    #[test]
    fn semver_order() {
        assert_eq!(cmp_semver("0.1.0", "0.1.0"), Ordering::Equal);
        assert_eq!(cmp_semver("0.10.0", "0.9.9"), Ordering::Greater);
        assert_eq!(cmp_semver("1.0.0", "1.0.0-rc.1"), Ordering::Greater);
        assert_eq!(cmp_semver("1.0.0-rc.2", "1.0.0-rc.10"), Ordering::Less);
        assert_eq!(cmp_semver("1.0.0-alpha", "1.0.0-alpha.1"), Ordering::Less);
        assert_eq!(cmp_semver("v2.0.0", "1.9.9"), Ordering::Greater);
        assert_eq!(cmp_semver("junk", "0.0.1"), Ordering::Less);
        assert!(parse_semver("1.2").is_none());
        assert!(parse_semver("1.2.3.4").is_none());
    }

    #[test]
    fn version_line() {
        assert_eq!(
            version_from_output("vibeke 0.2.1\n").as_deref(),
            Some("0.2.1")
        );
        assert_eq!(version_from_output("herdr 1.0.0"), None);
    }

    #[test]
    fn artifact_selection_picks_newest_for_target() {
        let d = tempfile::tempdir().unwrap();
        for (v, t) in [
            ("0.1.0", "macos-aarch64"),
            ("0.2.0", "macos-aarch64"),
            ("0.10.0", "linux-x86_64"),
            ("0.9.0", "macos-aarch64"),
            ("not-a-version", "macos-aarch64"),
        ] {
            let dir = d.path().join(v);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(format!("vibeke-{t}")), b"x").unwrap();
        }
        let (v, p) = newest_cached(d.path(), "macos-aarch64").unwrap();
        assert_eq!(v, "0.9.0");
        assert!(p.ends_with("0.9.0/vibeke-macos-aarch64"));
        assert_eq!(newest_cached(d.path(), "linux-x86_64").unwrap().0, "0.10.0");
        assert!(newest_cached(d.path(), "linux-aarch64").is_none());
        assert!(newest_cached(&d.path().join("missing"), "x").is_none());
    }

    #[test]
    fn report_rendering_and_exit() {
        let mut r = Report::default();
        r.add("install", Level::Pass, "binary ok");
        r.add_hint("install", Level::Warn, "no link", "run update");
        r.add("remote", Level::Info, "none");
        assert!(!r.failed());
        let t = r.render_text();
        assert!(
            t.contains("install\n  ok    binary ok\n  warn  no link\n        fix: run update\n")
        );
        assert!(t.contains("remote\n  info  none"));
        assert!(t.contains("1 ok, 1 warning(s), 0 failure(s)"));
        r.add("remote", Level::Fail, "down");
        assert!(r.failed());
        let j = r.to_json();
        assert_eq!(j["ok"], false);
        assert_eq!(j["summary"]["fail"], 1);
        assert_eq!(j["checks"][1]["hint"], "run update");
        assert_eq!(j["checks"][0]["level"], "pass");
    }

    #[test]
    fn permissions_and_shadows() {
        assert_eq!(perm_level(0o700), Level::Pass);
        assert_eq!(perm_level(0o750), Level::Fail);
        assert_eq!(perm_level(0o707), Level::Fail);
        assert_eq!(shadow_kind("codex is an alias for foo"), Some("alias"));
        assert_eq!(
            shadow_kind("codex is a shell function from /x"),
            Some("function")
        );
        assert_eq!(shadow_kind("codex is /usr/local/bin/codex"), None);
    }

    /// `vibeke doctor terminal` (03 §6.1): a pass/warn row per feature from the probe answers
    /// (with `terminal.host_overrides` applied), then the settings that use them.
    #[test]
    fn doctor_terminal_table() {
        use vk_tui::caps::{EnvHints, Notifications, Osc52, ProbeResult};
        let env = EnvHints {
            term: "xterm-ghostty".into(),
            term_program: "ghostty".into(),
            colorterm: "truecolor".into(),
            vte_or_wt: false,
        };
        let mut caps = ProbeResult {
            kitty_keyboard: true,
            sync_update: true,
            truecolor: true,
            osc52: Osc52::Allowed,
            xtversion: Some("ghostty 1.2".into()),
            da1: Some(vec![62, 22]),
            complete: true,
            undercurl: true,
            osc8: true,
            focus_events: true,
            sgr_mouse: true,
            bracketed_paste: true,
            notifications: Notifications::Osc777,
            ..ProbeResult::default()
        };
        let mut cfg = vk_config::Config::default();
        cfg.terminal
            .host_overrides
            .insert("kitty_graphics".into(), true);
        cfg.terminal.host_overrides.insert("osc8".into(), false);
        let rest = caps.apply_overrides(&cfg.terminal.host_overrides);
        let probe = TermProbe {
            caps,
            graphics: vk_browser::probe::GraphicsCaps {
                cell_px: Some((10, 21)),
                complete: true,
                ..Default::default()
            },
            graphics_overrides: rest,
        };
        let r = terminal_report(&env, Some(&probe), &cfg);
        let t = r.render_text();
        assert!(t.contains("ok    kitty keyboard protocol"), "{t}");
        assert!(
            t.contains("ok    kitty graphics (images, browser panes)"),
            "override applies: {t}"
        );
        assert!(t.contains("warn  OSC 8 hyperlinks: not detected"), "{t}");
        assert!(t.contains("ok    cell size: 10×21 px"));
        assert!(t.contains("warn  background: unknown"));
        assert!(t.contains("keys.altgr_mode: text"));
        assert!(t.contains("terminal.allow_passthrough: false"));
        assert!(t.contains("max_image_bytes 32MiB, max_total_per_pane 256MiB"));
        assert!(t.contains("host_overrides applied: kitty_graphics=true, osc8=false"));
        assert!(!r.failed());
        let j = r.to_json();
        assert!(
            j["checks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["section"] == "settings")
        );
        // Not a terminal: environment only, one warning saying why.
        let r = terminal_report(&env, None, &cfg);
        let t = r.render_text();
        assert!(
            t.contains("not probed: stdin/stdout is not a terminal"),
            "{t}"
        );
        assert!(t.contains("TERM=xterm-ghostty"));
    }

    #[test]
    fn terminal_host_hints() {
        assert!(host_hints("ghostty", None)[0].contains("clipboard-write"));
        assert!(
            host_hints("iTerm.app", None)
                .iter()
                .any(|h| h.contains("access clipboard"))
        );
        assert_eq!(host_hints("", Some("iTerm2 3.5")).len(), 2);
        assert!(host_hints("Apple_Terminal", None).is_empty());
    }

    #[test]
    fn install_switch_rollback_layout() {
        let d = tempfile::tempdir().unwrap();
        let l = Layout {
            data: d.path().join("share/vibeke"),
            bin: d.path().join("bin"),
        };
        std::fs::create_dir_all(&l.data).unwrap();
        let src = d.path().join("src");
        std::fs::write(&src, b"#!/bin/sh\n").unwrap();
        for v in ["0.1.0", "0.2.0", "0.3.0"] {
            install_version(&l, v, &src).unwrap();
            switch_current(&l, v).unwrap();
        }
        assert_eq!(l.current_version().as_deref(), Some("0.3.0"));
        assert_eq!(l.previous_version().as_deref(), Some("0.2.0"));
        assert!(l.version_bin("0.2.0").exists());
        assert!(!l.version_bin("0.1.0").exists(), "older versions pruned");
        assert!(l.bin.join("vibeke").exists());
        // Rollback swaps current and previous.
        switch_current(&l, "0.2.0").unwrap();
        assert_eq!(l.current_version().as_deref(), Some("0.2.0"));
        assert_eq!(l.previous_version().as_deref(), Some("0.3.0"));
        // Reinstalling the same version keeps the previous pointer.
        switch_current(&l, "0.2.0").unwrap();
        assert_eq!(l.previous_version().as_deref(), Some("0.3.0"));
    }

    /// Review batch 2, finding 10: `vibeke update` refuses an older candidate unless
    /// `--allow-downgrade`, and a signed candidate must be signed for its own version.
    #[test]
    fn update_refuses_downgrades_and_version_mismatched_signatures() {
        assert!(
            downgrade_allowed("0.9.0", "0.9.0", false).is_ok(),
            "reinstall"
        );
        assert!(
            downgrade_allowed("0.10.0", "0.9.0", false).is_ok(),
            "upgrade"
        );
        let e = downgrade_allowed("0.8.0", "0.9.0", false).unwrap_err();
        assert!(
            e.contains("refusing to downgrade from 0.9.0 to 0.8.0"),
            "{e}"
        );
        assert!(downgrade_allowed("0.9.0-rc.1", "0.9.0", false).is_err());
        assert!(downgrade_allowed("0.8.0", "0.9.0", true).is_ok());
        use vk_remote::bootstrap::{Trust, require_signed_version};
        assert!(require_signed_version(Trust::Signed, Some("vibeke v0.9.0"), "0.9.0").is_ok());
        let e = require_signed_version(Trust::Signed, Some("vibeke v0.8.0"), "0.9.0").unwrap_err();
        assert!(
            format!("{e:#}").contains("signed for \"vibeke v0.8.0\""),
            "{e:#}"
        );
        assert!(require_signed_version(Trust::Signed, None, "0.9.0").is_err());
        assert!(
            require_signed_version(Trust::UnsignedOptIn, None, "0.9.0").is_ok(),
            "unsigned opt-in has no signed version"
        );
    }

    #[test]
    fn candidate_requires_checksum_and_opt_in() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("vibeke-linux-x86_64");
        std::fs::write(&f, b"#!/bin/sh\ntouch executed\n").unwrap();
        // No sidecar: refused even with the opt-in (the checksum is never self-derived).
        assert!(vk_remote::bootstrap::trust_artifact(&f, true).is_err());
        let sha = vk_remote::bootstrap::sha256_file(&f).unwrap();
        std::fs::write(sidecar(&f), format!("{sha}  vibeke-linux-x86_64\n")).unwrap();
        assert!(vk_remote::bootstrap::trust_artifact(&f, false).is_err());
        assert!(vk_remote::bootstrap::trust_artifact(&f, true).is_ok());
        std::fs::write(sidecar(&f), "00  x\n").unwrap();
        assert!(vk_remote::bootstrap::trust_artifact(&f, true).is_err());
        assert!(!d.path().join("executed").exists());
    }
}
