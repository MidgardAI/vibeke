//! The run of one real, unchanged Herdr plugin (spec 18 §7.1, §8, §10, §11, §12), used by
//! `compat_herdr_real_plugins.rs`.
//!
//! This runs third-party plugin code. [`gate`] refuses to start unless `VIBEKE_HERDR_PLUGINS=1`
//! is set inside a disposable execution boundary (CI, or a machine that says so explicitly),
//! never on a developer machine (18 §5). Each run gets a fresh HOME, XDG dirs and TMPDIR, an
//! environment allowlist, and its own Vibeke session.
//!
//! One run: install from the `file://` mirror at the locked commit, check the commit and the
//! manifest sha256, trust (legacy) and build, activate, run the scenario's steps with positive
//! evidence for each (18 §8.2), compare the plugin pane grids (18 §10), then the lifecycle
//! checks (18 §8.3). The Herdr side runs only when `VIBEKE_HERDR_BIN` is set; it installs the
//! plugin with `herdr plugin install <owner/repo> --ref <commit> --yes` from the same mirror.
//!
//! The two sides run one after the other (18 §11): first every step on Vibeke, then every
//! process the Vibeke side started is killed and its ports are freed, then every step on
//! Herdr, and last the two sides' records are compared step by step. So a daemon one side
//! starts never answers the other side, and each side keeps its own state from step to step.
//! Both sides keep a real TUI client attached for the whole run, so both have a focused pane.
//! Every result goes into one `PluginResult` file; only harness errors, unexpected passes and
//! coverage failures fail the test (other outcomes are data for the report). Traces, server
//! logs, event logs and command logs go to `<results>/<stem>-<os>/logs/` for people; nothing
//! judges them.

use super::Session;
use super::plugins::{poll, read, write};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use vk_compat::herdr::manifest::{Manifest, PaneDecl};
use vk_compat::herdr::registry;
use vk_conformance::diff::{self, GridDiff, Placement};
use vk_conformance::grid::{Grid, Rect};
use vk_conformance::herdr::{self, HerdrOptions, HerdrSide, PopupSize};
use vk_conformance::lock::{DependencyMode, Lock, LockedPlugin};
use vk_conformance::outcome::{self, Outcome, SideResult, StepFacts};
use vk_conformance::pty::{PtyDriver, Spawn};
use vk_conformance::result::{
    CheckResult, CoverageSummary, GridArtifact, PluginResult, SCHEMA, StepResult, TEXT_MAX, clip,
};
use vk_conformance::scenario::{
    self, Compare, Expectation, LifecycleOp, Scenario, Step, StepKind, StubKind, Vars,
};
use vk_conformance::{coverage, logs, mask, render, trace};

/// The checkout this test binary was built from. A nextest archive runs on another machine,
/// where this path does not exist, so repository data comes from [`workspace_root`].
fn build_root() -> PathBuf {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    p.canonicalize().unwrap_or(p)
}

/// The repository root that holds the lock, scenarios, manifests and snapshots:
/// `$VIBEKE_HERDR_PLUGINS_ROOT` when set (the CI job's read-only copy of the repository data),
/// otherwise the checkout the test was built from.
pub fn workspace_root() -> PathBuf {
    match std::env::var_os("VIBEKE_HERDR_PLUGINS_ROOT") {
        Some(p) if !p.is_empty() => {
            let p = PathBuf::from(p);
            p.canonicalize().unwrap_or(p)
        }
        _ => build_root(),
    }
}

/// `tests/compat/herdr/0.9.3`.
pub fn baseline_dir() -> PathBuf {
    workspace_root().join("tests/compat/herdr/0.9.3")
}

pub fn plugins_dir() -> PathBuf {
    baseline_dir().join("plugins")
}

pub fn load_lock() -> Lock {
    let path = plugins_dir().join("top20.lock.toml");
    Lock::load(&path).unwrap_or_else(|e| panic!("{}: {e:#}", path.display()))
}

pub fn scenario_path(stem: &str) -> PathBuf {
    plugins_dir().join(format!("scenarios/{stem}.toml"))
}

/// The checked-in manifest of `lp` when its sha256 equals the lock's: `manifests/<stem>.toml`,
/// or the file `manifests/index.json` names for the plugin id. `None` when no checked-in file
/// matches the locked commit (the CI plugin job checks against the mirror instead).
pub fn checked_in_manifest(lock: &Lock, lp: &LockedPlugin) -> Option<Manifest> {
    let dir = baseline_dir().join("manifests");
    let mut candidates = vec![dir.join(format!("{}.toml", lp.stem(lock)))];
    let index: Value = serde_json::from_str(&read(&dir.join("index.json"))).unwrap_or_default();
    for m in index["manifests"].as_array().into_iter().flatten() {
        if m["plugin_id"] == lp.plugin_id.as_str()
            && let Some(f) = m["fixture"].as_str()
        {
            candidates.push(dir.join(f));
        }
    }
    candidates.into_iter().find_map(|p| {
        let bytes = std::fs::read(&p).ok()?;
        if registry::sha256_hex(&bytes) != lp.manifest_sha256 {
            return None;
        }
        Manifest::parse(std::str::from_utf8(&bytes).ok()?).ok()
    })
}

/// Where result files go: `$VIBEKE_HERDR_PLUGIN_RESULTS`, or `target/herdr-plugins` of the
/// build checkout (the repository data may be read-only).
pub fn results_dir() -> PathBuf {
    std::env::var_os("VIBEKE_HERDR_PLUGIN_RESULTS")
        .map(PathBuf::from)
        .unwrap_or_else(|| build_root().join("target/herdr-plugins"))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---- the gate (18 §5) -----------------------------------------------------------------------

pub struct Gate {
    pub mirror: PathBuf,
}

/// Refuse to run plugin code unless the job says it is inside an execution boundary.
pub fn gate() -> Gate {
    if std::env::var("VIBEKE_HERDR_PLUGINS").as_deref() != Ok("1") {
        panic!(
            "this test installs and runs an unchanged third-party Herdr plugin, so it runs only in \
             the herdr-plugins CI job, inside a disposable execution boundary (spec 18 §5). Set \
             VIBEKE_HERDR_PLUGINS=1 there and run it with `--run-ignored only`. No plugin code ran."
        );
    }
    let mirror = std::env::var_os("VIBEKE_PLUGIN_MIRROR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            panic!(
                "VIBEKE_PLUGIN_MIRROR=<dir> is required: the mirror that \
                 `bun scripts/herdr-plugins-fetch.ts --mirror <dir>` wrote"
            )
        });
    let ci = std::env::var("CI").as_deref() == Ok("true");
    let boundary = std::env::var("VIBEKE_HERDR_PLUGINS_I_AM_IN_A_BOUNDARY").as_deref() == Ok("1");
    if !ci && !boundary {
        panic!(
            "refusing to run third-party plugin code outside an execution boundary (spec 18 §5): \
             CI=true or VIBEKE_HERDR_PLUGINS_I_AM_IN_A_BOUNDARY=1 is required. Never run this on a \
             developer machine. No plugin code ran."
        );
    }
    assert!(
        mirror.is_absolute() && mirror.is_dir(),
        "VIBEKE_PLUGIN_MIRROR must be an absolute directory: {}",
        mirror.display()
    );
    Gate { mirror }
}

/// Inherited variables a plugin run may see (18 §5.3): no credentials, agent sockets, cloud
/// keys or tokens. `VIBEKE_HERDR_PLUGINS_ENV_ALLOW` (comma separated) adds names, such as
/// toolchain variables the workflow sets, but never secret-looking ones. The proxy and
/// certificate variables let build steps reach the dependency proxy (18 §7.2).
pub const ENV_ALLOW: &[&str] = &[
    "PATH",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TERM",
    "USER",
    "LOGNAME",
    "SHELL",
    "RUSTUP_HOME",
    "CARGO_HOME",
    "RUSTUP_TOOLCHAIN",
    "GOROOT",
    "GOTOOLCHAIN",
    "BUN_INSTALL",
    // The dependency proxy (18 §7.2).
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "https_proxy",
    "http_proxy",
    "all_proxy",
    "no_proxy",
    "SSL_CERT_FILE",
    "NODE_EXTRA_CA_CERTS",
    "CARGO_HTTP_CAINFO",
    "GIT_SSL_CAINFO",
    "REQUESTS_CA_BUNDLE",
    "CURL_CA_BUNDLE",
    "GOPROXY",
    "GOFLAGS",
    "GOSUMDB",
];

/// Colon-separated directories the workflow puts first on every plugin process's PATH, on
/// both sides: the pinned toolchains (18 §7.2).
pub const TOOLCHAIN_PATH_VAR: &str = "VIBEKE_HERDR_PLUGINS_TOOLCHAIN_PATH";

pub fn secret_like(name: &str) -> bool {
    let n = name.to_ascii_uppercase();
    [
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "CREDENTIAL",
        "AUTH",
        "KEY",
        "COOKIE",
        "SESSION",
    ]
    .iter()
    .any(|w| n.contains(w))
        || [
            "SSH_", "AWS_", "GITHUB_", "GH_", "ACTIONS_", "AZURE_", "GOOGLE_", "GCP_",
        ]
        .iter()
        .any(|p| n.starts_with(p))
}

/// A proxy URL with a user name or password in it (`http://user:pass@host`). Such a value
/// is a credential, so it is never passed on.
pub fn has_url_credentials(value: &str) -> bool {
    value.split(',').any(|part| {
        let rest = part
            .trim()
            .split_once("://")
            .map_or(part.trim(), |(_, r)| r);
        let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
        authority.contains('@')
    })
}

/// Names each side sets for its own isolation (a fresh HOME, XDG dirs and TMPDIR, and the
/// host's own variables). An extra allowlisted name never replaces them.
fn isolated(name: &str) -> bool {
    matches!(name, "HOME" | "TMPDIR")
        || ["XDG_", "VIBEKE", "HERDR"]
            .iter()
            .any(|p| name.to_ascii_uppercase().starts_with(p))
}

/// The inherited environment of plugin processes on both sides: the variables in `names`
/// that `get` knows, without secret-looking names or proxy values with credentials. PATH is
/// [`TOOLCHAIN_PATH_VAR`], then `path_dirs`, then the inherited PATH (a system default when
/// there is none).
pub fn plugin_env(
    names: &[String],
    get: impl Fn(&str) -> Option<String>,
    path_dirs: &[String],
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for n in names {
        if n == "PATH" || secret_like(n) || isolated(n) || out.iter().any(|(k, _)| k == n) {
            continue;
        }
        let Some(v) = get(n) else { continue };
        if n.to_ascii_uppercase().ends_with("_PROXY") && has_url_credentials(&v) {
            eprintln!("note: {n} holds credentials, so plugin processes do not get it");
            continue;
        }
        out.push((n.clone(), v));
    }
    let toolchain = get(TOOLCHAIN_PATH_VAR).unwrap_or_default();
    let inherited = names
        .iter()
        .any(|n| n == "PATH")
        .then(|| get("PATH"))
        .flatten()
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "/usr/bin:/bin:/usr/sbin:/sbin".into());
    let mut path: Vec<&str> = Vec::new();
    for dir in [toolchain.as_str()]
        .into_iter()
        .chain(path_dirs.iter().map(String::as_str))
        .chain([inherited.as_str()])
        .flat_map(|p| p.split(':'))
        .filter(|p| !p.is_empty())
    {
        if !path.contains(&dir) {
            path.push(dir);
        }
    }
    out.push(("PATH".into(), path.join(":")));
    out
}

/// The directories where tools that plugins install for their user end up, in the order a
/// real Herdr user's PATH usually has them: `~/.local/bin`, `~/.cargo/bin`, `~/go/bin` and
/// `~/.bun/bin` of the plugin's HOME, the `bin` dirs of `CARGO_HOME`, `GOPATH` and
/// `BUN_INSTALL` when `get` knows them, and on macOS Homebrew's and `/usr/local/bin`.
/// Plugin builds that run an installer (`curl … | sh`, `cargo install`, `go install`) then
/// find what they installed.
pub fn user_bin_dirs(home: &Path, get: impl Fn(&str) -> Option<String>, os: &str) -> Vec<String> {
    let mut dirs: Vec<String> = [".local/bin", ".cargo/bin", "go/bin", ".bun/bin"]
        .iter()
        .map(|d| home.join(d).display().to_string())
        .collect();
    for var in ["CARGO_HOME", "GOPATH", "BUN_INSTALL"] {
        if let Some(d) = get(var).filter(|d| !d.is_empty()) {
            dirs.push(Path::new(&d).join("bin").display().to_string());
        }
    }
    if os == "macos" {
        dirs.extend([
            "/opt/homebrew/bin".to_string(),
            "/usr/local/bin".to_string(),
        ]);
    }
    let mut out: Vec<String> = Vec::new();
    for d in dirs {
        if !out.contains(&d) {
            out.push(d);
        }
    }
    out
}

/// [`plugin_env`] of this process's environment, for the Vibeke side, whose plugin processes
/// have `home` as HOME.
pub fn inherited_plugin_env(home: &Path, os: &str) -> Vec<(String, String)> {
    let get = |n: &str| std::env::var(n).ok();
    plugin_env(&env_allowlist(), get, &user_bin_dirs(home, get, os))
}

/// Remove the directories in `exclude` from the `PATH` entry of `env` (a scenario's
/// `setup.path_exclude`, for example a package manager the plugin user cannot write to).
pub fn without_path_dirs(env: &mut [(String, String)], exclude: &[String]) {
    if exclude.is_empty() {
        return;
    }
    let norm = |d: &str| d.trim_end_matches('/').to_string();
    let drop: Vec<String> = exclude.iter().map(|d| norm(d)).collect();
    for (k, v) in env.iter_mut() {
        if k == "PATH" {
            *v = v
                .split(':')
                .filter(|d| !d.is_empty() && !drop.contains(&norm(d)))
                .collect::<Vec<_>>()
                .join(":");
        }
    }
}

/// The Vibeke side's config file (18 §11): no first-run setup guide and no update check, so
/// the TUI client shows the panes and nothing else, and Herdr's sidebar width, so both sides'
/// pane areas have the same size.
pub fn vibeke_config() -> String {
    format!(
        "onboarding = false\n\n[update]\nversion_check = false\nmanifest_check = false\n\n\
         [ui.sidebar]\nwidth = {}\nauto_width = false\n",
        herdr::SIDEBAR_WIDTH
    )
}

/// The invocation context of an action step, with Herdr's field names (`PluginInvocationContext`
/// in the 0.9.3 schema), the same on both sides. `global` leaves everything to the host (its
/// focused client); `pane` and `agent_pane` name the focused pane.
pub fn invocation_context(
    kind: Option<&str>,
    workspace: &str,
    tab: &str,
    pane: &str,
    agent_pane: Option<&str>,
) -> Value {
    match kind.unwrap_or("global") {
        "workspace" => json!({"workspace_id": workspace}),
        "tab" => json!({"workspace_id": workspace, "tab_id": tab}),
        "pane" | "selection" => {
            json!({"workspace_id": workspace, "tab_id": tab, "focused_pane_id": pane})
        }
        "agent_pane" => json!({
            "workspace_id": workspace,
            "tab_id": tab,
            "focused_pane_id": agent_pane.unwrap_or(pane),
        }),
        _ => json!({}),
    }
}

/// The `result` of a Herdr response line (`{"id": …, "result": {…}}`), which is what the
/// `herdr` command prints on both sides. A bare result (an older Vibeke shim) is returned as
/// it is.
pub fn herdr_result(v: Value) -> Value {
    match v {
        Value::Object(mut o)
            if o.contains_key("result") && (o.contains_key("id") || o.len() == 1) =>
        {
            o.remove("result").unwrap_or(Value::Null)
        }
        other => other,
    }
}

/// Where `url` is on a pane's visible text: (viewport row, a column inside the URL). The line
/// must show the URL itself, not quoted in a command line.
pub fn link_position(text: &str, url: &str) -> Option<(u16, u16)> {
    for (row, line) in text.lines().enumerate() {
        let mut from = 0;
        while let Some(i) = line[from..].find(url) {
            let at = from + i;
            let quoted = line[..at].ends_with('\'') || line[..at].ends_with('"');
            if !quoted {
                let col = line[..at].chars().count() + 1;
                return Some((u16::try_from(row).ok()?, u16::try_from(col).ok()?));
            }
            from = at + url.len();
        }
    }
    None
}

/// `s` quoted for a POSIX shell.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Whether a build failure looks like GitHub's limit for unauthenticated requests (shared
/// runner addresses hit it). Such a build is retried after a pause.
pub fn rate_limited(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    (t.contains("403") || t.contains("429"))
        && (t.contains("github") || t.contains("rate limit") || t.contains("api rate"))
}

/// The pauses before the retries of a rate-limited build.
const RATE_LIMIT_RETRIES: [Duration; 2] = [Duration::from_secs(60), Duration::from_secs(180)];

pub fn env_allowlist() -> Vec<String> {
    let extra = std::env::var("VIBEKE_HERDR_PLUGINS_ENV_ALLOW").unwrap_or_default();
    ENV_ALLOW
        .iter()
        .map(|s| s.to_string())
        .chain(
            extra
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        )
        .filter(|n| !secret_like(n))
        .collect()
}

// ---- stubs (18 §8) --------------------------------------------------------------------------

/// A local HTTP server for one `http_sink` stub: it records each request (method, path and
/// body) as a JSON line and answers `200 {}`.
fn http_sink(log: PathBuf) -> std::io::Result<String> {
    let l = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = l.local_addr()?.port();
    std::thread::spawn(move || {
        for conn in l.incoming() {
            let Ok(mut c) = conn else { continue };
            let log = log.clone();
            std::thread::spawn(move || {
                let _ = serve_one(&mut c, &log);
            });
        }
    });
    Ok(format!("http://127.0.0.1:{port}"))
}

static SINK_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serve_one(c: &mut std::net::TcpStream, log: &Path) -> std::io::Result<()> {
    c.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut r = std::io::BufReader::new(c.try_clone()?);
    let mut first = String::new();
    r.read_line(&mut first)?;
    let mut parts = first.split_whitespace();
    let (method, path) = (
        parts.next().unwrap_or("").to_string(),
        parts.next().unwrap_or("").to_string(),
    );
    let mut len = 0usize;
    loop {
        let mut h = String::new();
        if r.read_line(&mut h)? == 0 {
            break;
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':')
            && k.trim().eq_ignore_ascii_case("content-length")
        {
            len = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; len.min(1 << 20)];
    r.read_exact(&mut body)?;
    let line = json!({"method": method, "path": path, "body": String::from_utf8_lossy(&body)});
    {
        let _g = SINK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)?;
        writeln!(f, "{line}")?;
    }
    c.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
    )
}

/// The stubs of one side (18 §8): placeholder values, HTTP sink logs and stub environment.
struct Stubs {
    vars: Vars,
    sinks: BTreeMap<String, PathBuf>,
    env: Vec<(String, String)>,
}

/// Start the scenario's stubs for one side. `vars` holds the side's `{home}`, `{state}` and
/// `{config}`. Files without a path and the sink logs go under `dir`. Each side has its own
/// HTTP sinks, so a recorded request shows which side made it.
fn start_stubs(sc: &Scenario, mut vars: Vars, home: &Path, dir: &Path) -> Result<Stubs, String> {
    let mut sinks = BTreeMap::new();
    for st in &sc.stubs {
        let file = match st.kind {
            StubKind::HttpSink => {
                let log = dir.join(format!("{}.jsonl", st.id));
                std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
                std::fs::write(&log, "").map_err(|e| e.to_string())?;
                let url = http_sink(log.clone()).map_err(|e| e.to_string())?;
                vars.values.insert(format!("stub.{}.url", st.id), url);
                sinks.insert(st.id.clone(), log.clone());
                Some(log)
            }
            StubKind::File => {
                let p = st
                    .path
                    .as_deref()
                    .map(|p| scenario::expand(p, &vars).map_err(|e| format!("{e:#}")))
                    .transpose()?
                    .map(|p| home.join(p))
                    .unwrap_or_else(|| dir.join(&st.id));
                let text = st
                    .contents
                    .as_deref()
                    .map(|c| scenario::expand(c, &vars).map_err(|e| format!("{e:#}")))
                    .transpose()?
                    .unwrap_or_default();
                write(&p, &text);
                Some(p)
            }
            StubKind::Env => None,
        };
        if let Some(f) = file {
            vars.values.insert(
                format!("stub.{}.file", st.id),
                f.to_string_lossy().into_owned(),
            );
        }
    }
    let mut env = Vec::new();
    for st in &sc.stubs {
        for (k, v) in &st.env {
            env.push((
                k.clone(),
                scenario::expand(v, &vars).map_err(|e| format!("stub {}: {e:#}", st.id))?,
            ));
        }
    }
    Ok(Stubs { vars, sinks, env })
}

/// Herdr's `{home}`, `{state}`, `{config}` and `{herdr_config}` under the Herdr side's root ([`HerdrOptions`]
/// root: `home`, and XDG config and state dirs `config` and `state`). At the pinned commit a
/// plugin's config dir is `$XDG_CONFIG_HOME/herdr/plugins/config/<id>` and its state dir
/// `$XDG_STATE_HOME/herdr/plugins/<id>` (`src/plugin_paths.rs`), with the id escaped.
fn herdr_vars(root: &Path, plugin_id: &str) -> Result<Vars, String> {
    let c = herdr_path_component(plugin_id).ok_or_else(|| {
        format!("Herdr escapes the plugin id {plugin_id:?} in its dir names; the harness does not")
    })?;
    let mut vars = Vars::default();
    for (k, v) in [
        ("home", root.join("home")),
        ("state", root.join("state/herdr/plugins").join(&c)),
        ("config", root.join("config/herdr/plugins/config").join(&c)),
        ("herdr_config", herdr_config_file(root)),
    ] {
        vars.values
            .insert(k.into(), v.to_string_lossy().into_owned());
    }
    Ok(vars)
}

/// The config file of the Herdr side (`$XDG_CONFIG_HOME/herdr/config.toml`), which
/// [`HerdrSide::start`] writes.
fn herdr_config_file(root: &Path) -> PathBuf {
    root.join("config/herdr/config.toml")
}

/// Herdr's dir name for a plugin id, for ids it keeps unchanged: lowercase ASCII letters,
/// digits, `.`, `_` and `-`, not ending in `.`, at most 120 characters and without a Windows
/// reserved stem. `None` for any other id (Herdr escapes or hashes those).
pub fn herdr_path_component(id: &str) -> Option<String> {
    let plain = !id.is_empty()
        && id.len() <= 120
        && !id.ends_with('.')
        && id.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        });
    let stem = id.split('.').next().unwrap_or("");
    let reserved = matches!(stem, "con" | "prn" | "aux" | "nul")
        || ((stem.starts_with("com") || stem.starts_with("lpt"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit());
    (plain && !reserved).then(|| id.to_string())
}

fn git_try(dir: &Path, args: &[&str]) -> Result<String, String> {
    let o = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .map_err(|e| format!("git {args:?}: {e}"))?;
    if o.status.success() {
        Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
    } else {
        Err(format!(
            "git {args:?}: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        ))
    }
}

// ---- results --------------------------------------------------------------------------------

/// One lifecycle check. Its reason is cut to fit the report (a failed build can print
/// megabytes).
fn check(name: &str, outcome: Outcome, reason: impl Into<Option<String>>) -> CheckResult {
    CheckResult {
        check: name.to_string(),
        outcome,
        reason: reason.into().map(|r| clip(&r, TEXT_MAX)),
    }
}

/// The reason of a step: the outcome's own reason (or the known reason), then each side's
/// failure reason, so the report shows why Vibeke failed and what Herdr did. Each part gets a
/// share of the report's text limit.
pub fn combine_reason(
    head: Option<&str>,
    vibeke: Option<&str>,
    herdr: Option<&str>,
) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(h) = head.filter(|h| !h.is_empty()) {
        parts.push(clip(h, 400));
    }
    if let Some(v) = vibeke.filter(|v| !v.is_empty()) {
        parts.push(format!("vibeke: {}", clip(v, 800)));
    }
    // Rule 4 of the attribution already quotes Herdr's reason in the head.
    if let Some(h) = herdr.filter(|h| !h.is_empty() && !head.is_some_and(|x| x.contains(*h))) {
        parts.push(format!("herdr: {}", clip(h, 500)));
    }
    (!parts.is_empty()).then(|| clip(&parts.join("; "), TEXT_MAX))
}

/// What a finished plugin invocation record (`plugin.log.list`, on either side) says about
/// itself: status, exit code, error and the last part of its output.
pub fn invocation_details(l: &Value) -> String {
    let tail = |k: &str| {
        let t = l[k].as_str().unwrap_or_default().trim();
        let n = t.chars().count();
        t.chars().skip(n.saturating_sub(400)).collect::<String>()
    };
    let mut out = format!(
        "invocation {} {} (exit code {})",
        l["log_id"].as_str().unwrap_or("?"),
        l["status"].as_str().unwrap_or("?"),
        l["exit_code"]
    );
    if let Some(e) = l["error"].as_str().filter(|e| !e.is_empty()) {
        out.push_str(&format!("; error: {e}"));
    }
    for k in ["stderr", "stdout"] {
        let t = tail(k);
        if !t.is_empty() {
            out.push_str(&format!("; {k} tail: {t}"));
        }
    }
    clip(&out, TEXT_MAX)
}

/// The `herdr plugin pane open` arguments of a pane step (Herdr 0.9.3 takes only flags).
/// Overlay and popup panes target the active pane and refuse a target; split and zoomed
/// panes take `--target-pane`, the pane Vibeke's side targets too.
pub fn herdr_pane_open_args(
    plugin_id: &str,
    entrypoint: &str,
    placement: Option<&str>,
    target: Option<&str>,
) -> Vec<String> {
    let mut a: Vec<String> = [
        "plugin",
        "pane",
        "open",
        "--plugin",
        plugin_id,
        "--entrypoint",
        entrypoint,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if let (Some("split" | "zoomed"), Some(t)) = (placement, target) {
        a.extend(["--target-pane".to_string(), t.to_string()]);
    }
    a
}

/// The `herdr plugin install` arguments: `owner/repo[/subdir] --ref <commit> --yes` (stdin is
/// not a terminal, so Herdr requires `--yes`).
pub fn herdr_install_args(repository: &str, subdir: Option<&str>, commit: &str) -> Vec<String> {
    let source = match subdir {
        Some(d) => format!("{repository}/{d}"),
        None => repository.to_string(),
    };
    vec![
        "plugin".into(),
        "install".into(),
        source,
        "--ref".into(),
        commit.into(),
        "--yes".into(),
    ]
}

fn passed(name: &str) -> CheckResult {
    check(name, Outcome::Passed, None)
}

fn kind_name(k: StepKind) -> String {
    serde_json::to_value(k)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn empty_result(lp: &LockedPlugin, os: &str) -> PluginResult {
    PluginResult {
        schema: SCHEMA.to_string(),
        plugin_id: lp.plugin_id.clone(),
        repository: lp.repository.clone(),
        rank: lp.rank,
        os: os.to_string(),
        commit: lp.commit.clone(),
        dependency_mode: lp.dependency_mode,
        reproducible: lp.dependency_mode == DependencyMode::Verified,
        outcome: Outcome::Passed,
        fails_run: false,
        surfaces: vec![],
        coverage: CoverageSummary {
            ok: true,
            missing: vec![],
            skipped: vec![],
        },
        lifecycle: vec![],
        steps: vec![],
        duration_ms: 0,
    }
}

/// A result with one step that explains why nothing ran.
fn single_step(lp: &LockedPlugin, os: &str, outcome: Outcome, reason: &str) -> PluginResult {
    let mut r = empty_result(lp, os);
    r.steps.push(StepResult {
        id: "harness".into(),
        kind: "wait".into(),
        outcome,
        vibeke: if outcome == Outcome::HarnessError {
            SideResult::HarnessError
        } else {
            SideResult::NotRun
        },
        herdr: SideResult::NotRun,
        known: None,
        unexpected_pass: false,
        reason: Some(reason.to_string()),
        evidence: vec![],
        grids: vec![],
        duration_ms: 0,
    });
    r
}

fn finish(r: &mut PluginResult) {
    r.outcome = outcome::worst(
        r.lifecycle
            .iter()
            .map(|c| c.outcome)
            .chain(r.steps.iter().map(|s| s.outcome)),
    );
    r.fails_run = !r.coverage.ok || r.steps.iter().any(|s| s.unexpected_pass);
}

fn write_result(stem: &str, r: &PluginResult) -> PathBuf {
    let dir = results_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{stem}-{}.json", r.os));
    std::fs::write(&path, r.to_json_redacted()).unwrap();
    path
}

/// The test fails only on harness errors, unexpected passes and coverage failures.
fn judge(r: &PluginResult, path: &Path) {
    let mut why = Vec::new();
    for c in &r.lifecycle {
        if c.outcome == Outcome::HarnessError {
            why.push(format!(
                "harness error in check {}: {:?}",
                c.check, c.reason
            ));
        }
    }
    for s in &r.steps {
        if s.outcome == Outcome::HarnessError {
            why.push(format!("harness error in step {}: {:?}", s.id, s.reason));
        }
        if s.unexpected_pass {
            why.push(format!(
                "step {} passes but the scenario lists a known result: update the scenario",
                s.id
            ));
        }
    }
    if !r.coverage.ok {
        why.push(format!(
            "coverage check failed (18 §8.1): {:?}",
            r.coverage.missing
        ));
    }
    assert!(
        why.is_empty(),
        "{} ({}):\n{}\nresult: {}",
        r.plugin_id,
        r.os,
        why.join("\n"),
        path.display()
    );
    eprintln!(
        "{} ({}): {:?}, result in {}",
        r.plugin_id,
        r.os,
        r.outcome,
        path.display()
    );
}

/// Run the locked plugin `rank` (whose stem must be `stem`), write its result file and judge it.
pub fn run_locked(rank: u32, stem: &str) {
    let gate = gate();
    let lock = load_lock();
    let lp = lock
        .plugins
        .iter()
        .find(|p| p.rank == rank)
        .unwrap_or_else(|| panic!("no locked plugin with rank {rank}"));
    assert_eq!(
        lp.stem(&lock),
        stem,
        "the test list is stale: update `locked_plugins!` from the lock"
    );
    let os = vk_compat::herdr::current_platform();
    let t0 = Instant::now();
    let mut result = if let Some(d) = lock.deferral(lp) {
        // The matrix leaves deferred plugins out; a direct run says why and runs nothing.
        single_step(
            lp,
            os,
            Outcome::NotExercised,
            &format!("deferred (18 §6): {}", d.reason),
        )
    } else if !lp.supports(os) {
        single_step(
            lp,
            os,
            Outcome::NotExercised,
            &format!("the plugin does not declare {os}"),
        )
    } else {
        let path = scenario_path(stem);
        if !path.exists() {
            single_step(lp, os, Outcome::HarnessError, "no scenario")
        } else {
            match Scenario::load(&path) {
                Err(e) => single_step(
                    lp,
                    os,
                    Outcome::HarnessError,
                    &format!("scenario {}: {e:#}", path.display()),
                ),
                Ok(sc) => match Run::new(&gate, &lock, lp, &sc, os) {
                    Ok(mut run) => run.run(),
                    Err(e) => single_step(lp, os, Outcome::HarnessError, &e),
                },
            }
        }
    };
    result.duration_ms = t0.elapsed().as_millis() as u64;
    finish(&mut result);
    let path = write_result(stem, &result);
    judge(&result, &path);
}

// ---- one run --------------------------------------------------------------------------------

/// Why a side failed a step: the plugin or Vibeke (`Fail`), or the harness (`Harness`).
enum StepErr {
    Fail(String),
    Harness(String),
}

type StepRes<T> = Result<T, StepErr>;

fn fail<T>(why: impl Into<String>) -> StepRes<T> {
    Err(StepErr::Fail(why.into()))
}

fn harness<T>(why: impl Into<String>) -> StepRes<T> {
    Err(StepErr::Harness(why.into()))
}

/// The workspace every step runs in (Herdr-style ids, which Vibeke also accepts).
#[derive(Default)]
struct Context {
    workspace: String,
    tab: String,
    pane: String,
    agent_pane: Option<String>,
}

/// One side's grid of one plugin pane after a step, for the comparison and the screenshots.
#[derive(Default)]
struct Capture {
    /// The grid file, relative to the results directory.
    file: Option<String>,
    /// The masked, redacted grid.
    grid: Option<Grid>,
    /// The pane's outer rectangle relative to the side's pane area, when the side reports it.
    rect: Option<Rect>,
    /// The placement the side shows: `split`, `tab`, `zoomed`, `overlay` or `popup`.
    placement: Option<String>,
    /// What is missing from the capture, and why.
    notes: Vec<String>,
}

/// What one side did with one step. The comparison of the two sides happens after both ran
/// ([`Run::merge_step`]).
struct SideStep {
    result: SideResult,
    reason: Option<String>,
    evidence: Vec<String>,
    /// Vibeke only: failures of the Vibeke-only checks of compared panes (the pane was not
    /// found, never became stable, or differs from its reference snapshot).
    compare_why: Vec<String>,
    /// Captures by pane entrypoint.
    captures: BTreeMap<String, Capture>,
    duration_ms: u64,
}

impl SideStep {
    fn not_run(why: &str) -> SideStep {
        SideStep {
            result: SideResult::NotRun,
            reason: Some(why.to_string()),
            evidence: vec![],
            compare_why: vec![],
            captures: BTreeMap::new(),
            duration_ms: 0,
        }
    }
}

struct Run<'a> {
    gate: &'a Gate,
    lock: &'a Lock,
    lp: &'a LockedPlugin,
    sc: &'a Scenario,
    os: &'static str,
    stem: String,
    /// HOME, trace, stub logs and the mirror clone live here.
    root: tempfile::TempDir,
    s: Session,
    trace: PathBuf,
    trace_off: u64,
    /// The last event sequence number seen.
    seq: i64,
    vars: Vars,
    ctx: Context,
    sinks: BTreeMap<String, PathBuf>,
    /// The Herdr side's stubs, or why they could not be set up.
    herdr_stubs: Result<Stubs, String>,
    herdr_root: PathBuf,
    /// Herdr plugin panes the steps opened or found: entrypoint to Herdr pane id.
    herdr_panes: BTreeMap<String, String>,
    manifest: Option<Manifest>,
    plugin_root: Option<PathBuf>,
    state_dir: Option<PathBuf>,
    config_dir: Option<PathBuf>,
    clone: Option<PathBuf>,
    /// Server activations since install (startup commands run once per activation).
    activations: u32,
    /// Startup invocations recorded before this run's first activation.
    startup_base: usize,
    build_failed: Option<String>,
    herdr: Option<HerdrSide>,
    herdr_trace: PathBuf,
    herdr_off: u64,
    herdr_note: String,
    /// Herdr's context workspace, tab and pane (the TUI's focused pane at start) and its fake
    /// agent pane.
    herdr_workspace: Option<String>,
    herdr_tab: Option<String>,
    herdr_pane: Option<String>,
    herdr_agent_pane: Option<String>,
    /// The `events.subscribe` log of the Herdr side, the offset where the current step's
    /// events start, and why the subscription failed, if it did.
    herdr_events: PathBuf,
    herdr_events_off: u64,
    herdr_events_err: Option<String>,
    /// The entrypoint whose popup Herdr shows, from the `plugin.pane.open` calls in its trace.
    herdr_popup: Option<String>,
    /// The Herdr pane ids at the start of the current step.
    herdr_before: BTreeSet<String>,
    /// The Vibeke TUI client attached for the whole Vibeke phase (18 §11: both sides have a
    /// focused client), or `None` before `context`.
    tui: Option<PtyDriver>,
    /// The grid files of this run: `<results>/<stem>-<os>/`.
    grid_dir: PathBuf,
    grid_rel: String,
    /// Grid files each side wrote, and grid entries in the result (the screenshot renderer
    /// reads at most `vk_conformance::shots::MAX_GRIDS`).
    vibeke_grids: usize,
    herdr_grids: usize,
    grid_count: usize,
    lifecycle: Vec<CheckResult>,
}

/// The size of both sides' TUI clients.
const TUI_COLS: u16 = 120;
const TUI_ROWS: u16 = 40;
/// Grid entries one result may hold, under the screenshot renderer's limit.
const MAX_GRIDS: usize = vk_conformance::shots::MAX_GRIDS - 4;
/// How long `herdr plugin install` may take: it runs the plugin's build commands.
const HERDR_INSTALL_TIMEOUT: Duration = Duration::from_secs(20 * 60);

/// Placements whose size the host decides (Herdr cannot resize them, 18 §10.2): both sides
/// compare them at the size they have.
fn fixed_size(placement: &str) -> bool {
    matches!(placement, "popup" | "overlay" | "zoomed" | "tab")
}

/// The placement kind used to compare the sides: an overlay is a zoomed split on Herdr.
fn placement_kind(placement: &str) -> String {
    match placement {
        "overlay" => "zoomed".into(),
        other => other.into(),
    }
}

impl<'a> Run<'a> {
    fn new(
        gate: &'a Gate,
        lock: &'a Lock,
        lp: &'a LockedPlugin,
        sc: &'a Scenario,
        os: &'static str,
    ) -> Result<Run<'a>, String> {
        let root = tempfile::Builder::new()
            .prefix("vkhr")
            .tempdir_in("/tmp")
            .map_err(|e| e.to_string())?;
        let rp = root.path().canonicalize().map_err(|e| e.to_string())?;
        let home = rp.join("home");
        std::fs::create_dir_all(&home).map_err(|e| e.to_string())?;
        let trace = rp.join("trace.jsonl");
        let inherited = inherited_plugin_env(&home, os);
        let s = Session::builder()
            .prefix("vkhp")
            .home(&home)
            // No first-run guide, no update check, Herdr's sidebar width (18 §11).
            .config(&vibeke_config())
            // Nothing is copied by name: `inherited` is the filtered environment (18 §5.3).
            .allow_only(&[])
            .env("VIBEKE_TEST_HOOKS", "1")
            .env(
                "VIBEKE_PLUGIN_GIT_BASE",
                format!("file://{}", gate.mirror.display()),
            )
            .env("VIBEKE_HERDR_TRACE", &trace)
            .build();
        // The plugin's dirs are fixed by the session layout, so `{state}` and `{config}` are
        // known before install, and file stubs can live there (07 §7.7 plugin dirs).
        let state = s.path(&format!("state/plugins/state/{}", lp.plugin_id));
        let config = s.path(&format!("plugins/{}", lp.plugin_id));
        for d in [&state, &config] {
            std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
        let mut vars = Vars::default();
        for (k, v) in [
            ("home", home.clone()),
            ("state", state.clone()),
            ("config", config.clone()),
            // The Herdr config file of the Vibeke side's `herdr` command: Herdr's default
            // location under the plugin's XDG config dir.
            ("herdr_config", home.join(".config/herdr/config.toml")),
        ] {
            vars.values
                .insert(k.into(), v.to_string_lossy().into_owned());
        }
        // Stubs: their URLs and files are placeholders in the session's environment.
        let stubs = start_stubs(sc, vars, &home, &rp.join("stubs"))?;
        // The Herdr side gets its own stubs, with Herdr's dirs as `{home}`, `{state}` and
        // `{config}`. If they cannot be set up, Herdr does not run (18 §11).
        let herdr_root = rp.join("herdr");
        let herdr_home = herdr_root.join("home");
        let herdr_stubs = herdr_vars(&herdr_root, &lp.plugin_id)
            .and_then(|v| start_stubs(sc, v, &herdr_home, &herdr_root.join("stubs")))
            .map_err(|e| format!("Herdr side not set up: {e}"));
        let Stubs {
            vars,
            sinks,
            env: stub_env,
        } = stubs;
        let mut s = s;
        let mut env = inherited;
        env.extend(std::mem::take(&mut s.env));
        env.extend(stub_env);
        without_path_dirs(&mut env, &sc.setup.path_exclude);
        s.env = env;
        let stem = lp.stem(lock);
        let grid_rel = format!("{stem}-{os}");
        Ok(Run {
            gate,
            lock,
            lp,
            sc,
            os,
            grid_dir: results_dir().join(&grid_rel),
            grid_rel,
            stem,
            herdr_trace: rp.join("herdr-trace.jsonl"),
            herdr_events: rp.join("herdr-events.jsonl"),
            root,
            s,
            trace,
            trace_off: 0,
            seq: 0,
            vars,
            ctx: Context::default(),
            sinks,
            herdr_stubs,
            herdr_root,
            herdr_panes: BTreeMap::new(),
            manifest: None,
            plugin_root: None,
            state_dir: Some(state),
            config_dir: Some(config),
            clone: None,
            activations: 0,
            startup_base: 0,
            build_failed: None,
            herdr: None,
            herdr_off: 0,
            herdr_note: "VIBEKE_HERDR_BIN is not set: Herdr has not run this step".into(),
            herdr_workspace: None,
            herdr_tab: None,
            herdr_pane: None,
            herdr_agent_pane: None,
            herdr_events_off: 0,
            herdr_events_err: None,
            herdr_popup: None,
            herdr_before: BTreeSet::new(),
            tui: None,
            vibeke_grids: 0,
            herdr_grids: 0,
            grid_count: 0,
            lifecycle: vec![],
        })
    }

    fn id(&self) -> &str {
        &self.lp.plugin_id
    }

    fn home(&self) -> PathBuf {
        self.root.path().canonicalize().unwrap().join("home")
    }

    fn expand(&self, s: &str) -> StepRes<String> {
        scenario::expand(s, &self.vars).map_err(|e| StepErr::Harness(format!("{e:#}")))
    }

    /// Run `vibeke <args>`: Ok(stdout JSON) or Err(stderr text).
    fn cli(&self, args: &[&str]) -> Result<Value, String> {
        let out = self
            .s
            .cmd(args)
            .output()
            .map_err(|e| format!("run vibeke: {e}"))?;
        if out.status.success() {
            Ok(serde_json::from_slice(&out.stdout).unwrap_or(Value::Null))
        } else {
            Err(format!(
                "{}{}",
                String::from_utf8_lossy(&out.stderr).trim(),
                String::from_utf8_lossy(&out.stdout).trim()
            ))
        }
    }

    fn api(&self, method: &str, params: Value) -> Result<Value, String> {
        self.cli(&["api", "call", method, &params.to_string()])
    }

    /// The Herdr compatibility socket of the Vibeke side: Ok(result) or Err(the error).
    fn compat_call(&self, method: &str, params: Value) -> Result<Value, String> {
        let r = self.api(
            "compat.herdr.call",
            json!({"method": method, "params": params}),
        )?;
        if !r["error"].is_null() {
            return Err(format!("{method}: {}", r["error"]));
        }
        Ok(herdr_result(r))
    }

    /// `herdr <args>` on the Vibeke side. It prints Herdr's response line; this returns its
    /// `result`.
    fn herdr_cli(&self, args: &[String]) -> Result<Value, String> {
        let mut a = vec!["compat", "herdr"];
        a.extend(args.iter().map(String::as_str));
        self.cli(&a).map(herdr_result)
    }

    /// The error `kind` (or Herdr `code`) of a failed command's JSON stderr.
    fn error_kind(text: &str) -> String {
        text.lines()
            .find_map(|l| serde_json::from_str::<Value>(l).ok())
            .and_then(|v| {
                v["error"]["kind"]
                    .as_str()
                    .or(v["error"]["code"].as_str())
                    .map(str::to_string)
            })
            .unwrap_or_default()
    }

    fn logs(&self) -> Vec<Value> {
        self.api(
            "plugin.log.list",
            json!({"plugin": self.lp.plugin_id, "limit": 500}),
        )
        .ok()
        .and_then(|v| v["logs"].as_array().cloned())
        .unwrap_or_default()
    }

    fn startup_logs(&self) -> Vec<Value> {
        self.logs()
            .into_iter()
            .filter(|l| l["source"] == "startup")
            .collect()
    }

    fn startup_count(&self) -> usize {
        self.manifest
            .as_ref()
            .map(|m| m.startup_on(self.os).len())
            .unwrap_or(0)
    }

    /// Events after the last seen sequence number; moves the cursor.
    fn new_events(&mut self) -> Vec<Value> {
        let mut out = Vec::new();
        loop {
            let Ok(v) = self.api("events.read", json!({"after": self.seq, "limit": 500})) else {
                break;
            };
            let evs = v["events"].as_array().cloned().unwrap_or_default();
            let Some(last) = evs.last().and_then(|e| e["seq"].as_i64()) else {
                break;
            };
            self.seq = last;
            out.extend(evs);
        }
        out
    }

    fn events_since(&self, seq: i64, kind: &str) -> Vec<Value> {
        self.api(
            "events.read",
            json!({"after": seq, "types": kind, "limit": 500}),
        )
        .ok()
        .and_then(|v| v["events"].as_array().cloned())
        .unwrap_or_default()
    }

    /// Trace lines of this plugin from byte `from`; returns them and the new offset.
    fn trace_since(&self, from: u64) -> StepRes<(Vec<trace::TraceLine>, u64)> {
        trace::read(&self.trace, from)
            .map(|(l, off)| {
                (
                    l.into_iter()
                        .filter(|x| x.plugin == self.lp.plugin_id)
                        .collect(),
                    off,
                )
            })
            .map_err(|e| StepErr::Harness(format!("trace: {e:#}")))
    }

    /// Kill the server and start it again. The TUI client goes first and is attached again
    /// afterwards, so the client never races the restart.
    fn restart(&mut self) {
        let attached = self.detach_client();
        if self.s.server_pid() > 0 {
            self.s.kill_server();
        }
        let _ = self.cli(&["pane", "list"]);
        self.activations += 1;
        if attached && let Err(e) = self.attach_client() {
            eprintln!("note: {e}");
        }
    }

    /// Attach a real Vibeke TUI client in a PTY (`vibeke attach`, the session's environment,
    /// `TERM=xterm-256color`, a fixed size) and focus the context pane in it. Vibeke's compat
    /// layer takes the focused pane from the most recently active TUI client, as Herdr takes it
    /// from its own client, so global and workspace actions get the same focus on both sides.
    fn attach_client(&mut self) -> Result<(), String> {
        self.detach_client();
        let c = self.s.cmd(&[]);
        let program = PathBuf::from(c.get_program());
        let mut env: Vec<(String, String)> = c
            .get_envs()
            .filter_map(|(k, v)| Some((k.to_str()?.to_string(), v?.to_str()?.to_string())))
            .filter(|(k, _)| k != "TERM")
            .collect();
        env.push(("TERM".into(), "xterm-256color".into()));
        let tui = PtyDriver::spawn(&Spawn {
            program: &program,
            args: &["attach".to_string()],
            cwd: &self.home(),
            env: &env,
            cols: TUI_COLS,
            rows: TUI_ROWS,
        })
        .map_err(|e| format!("attach a Vibeke TUI client: {e:#}"))?;
        self.tui = Some(tui);
        self.focus_client()
    }

    /// Focus the context pane in the attached TUI client.
    fn focus_client(&self) -> Result<(), String> {
        let pane = self.ctx.pane.clone();
        let mut last = String::new();
        let focused = poll(Duration::from_secs(30), || {
            match self.api("client.focus", json!({"pane": pane, "raise": false})) {
                Ok(r) if r["focused"] == true => true,
                Ok(r) => {
                    last = r.to_string();
                    false
                }
                Err(e) => {
                    last = e;
                    false
                }
            }
        });
        if focused {
            Ok(())
        } else {
            let screen = self.client_screen().unwrap_or_default();
            Err(format!(
                "the Vibeke TUI client did not take focus of {pane}: {last}\nscreen:\n{screen}"
            ))
        }
    }

    /// Shut the TUI client down; returns whether one was attached.
    fn detach_client(&mut self) -> bool {
        match self.tui.take() {
            Some(t) => {
                let _ = t.shutdown(Duration::from_secs(2));
                true
            }
            None => false,
        }
    }

    /// The text on the Vibeke TUI client's screen.
    fn client_screen(&self) -> Option<String> {
        let (rows, cols, nrows, _) = self.tui.as_ref()?.snapshot();
        Some(Grid::from_rows(&rows, cols, nrows).text())
    }

    /// Wait until every startup command of every activation since install has a record.
    fn wait_startup(&self, timeout: Duration) -> bool {
        let want = self.startup_base + self.startup_count() * self.activations as usize;
        poll(timeout, || self.startup_logs().len() >= want)
    }

    fn step_timeout(&self, step: Option<&Step>) -> Duration {
        Duration::from_millis(
            step.and_then(|s| s.timeout_ms)
                .or(self.sc.timeout_ms)
                .unwrap_or(120_000),
        )
    }

    /// A clone of the mirror at the locked commit; returns the plugin directory inside it.
    fn clone_dir(&mut self) -> Result<PathBuf, String> {
        if self.clone.is_none() {
            let dst = self.root.path().canonicalize().unwrap().join("clone");
            let src = self.gate.mirror.join(&self.lp.repository);
            git_try(
                self.root.path(),
                &["clone", "-q", src.to_str().unwrap(), dst.to_str().unwrap()],
            )?;
            git_try(&dst, &["fetch", "-q", "origin", &self.lp.commit])?;
            git_try(&dst, &["checkout", "-q", "--detach", &self.lp.commit])?;
            self.clone = Some(dst);
        }
        let c = self.clone.clone().unwrap();
        Ok(match self.lp.subdir() {
            Some(d) => c.join(d),
            None => c,
        })
    }

    // ---- setup ------------------------------------------------------------------------------

    fn context(&mut self) -> Result<(), String> {
        let home = self.home();
        self.create_context(&home)?;
        self.attach_client()?;
        self.new_events();
        Ok(())
    }

    /// A workspace in `cwd` (Herdr's `workspace create --cwd`), its root pane as the context
    /// pane, and a fake agent pane split below it when the scenario asks for one.
    fn create_context(&mut self, cwd: &Path) -> Result<(), String> {
        let w = self.herdr_cli(&[
            "workspace".into(),
            "create".into(),
            "--cwd".into(),
            cwd.to_string_lossy().into_owned(),
        ])?;
        self.ctx.workspace = w["workspace"]["workspace_id"]
            .as_str()
            .unwrap_or_default()
            .into();
        self.ctx.tab = w["tab"]["tab_id"].as_str().unwrap_or_default().into();
        self.ctx.pane = w["root_pane"]["pane_id"]
            .as_str()
            .unwrap_or_default()
            .into();
        if self.ctx.pane.is_empty() {
            return Err(format!("workspace create: no root pane in {w}"));
        }
        self.ctx.agent_pane = None;
        if self.sc.setup.fake_agent {
            let p = self.herdr_cli(&[
                "pane".into(),
                "split".into(),
                self.ctx.pane.clone(),
                "--direction".into(),
                "down".into(),
            ])?;
            let id = p["pane"]["pane_id"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            if id.is_empty() {
                return Err(format!("fake agent pane: {p}"));
            }
            self.vars.values.insert("agent_pane".into(), id.clone());
            self.ctx.agent_pane = Some(id);
        }
        for (k, v) in [
            ("workspace", &self.ctx.workspace),
            ("tab", &self.ctx.tab),
            ("pane", &self.ctx.pane),
        ] {
            self.vars.values.insert(k.into(), v.clone());
        }
        Ok(())
    }

    /// Repositories, worktrees, files and plugin config from `[setup]`. Runs after install and
    /// before trust, so `{state}` is known and no plugin code has run yet. With
    /// `setup.workspace_cwd`, the context workspace is then created again in that directory.
    fn setup_files(&mut self) -> StepRes<()> {
        let home = self.home();
        let config = self.config_dir.clone();
        materialize_setup(
            self.sc,
            &self.lp.plugin_id,
            &home,
            &mut self.vars,
            config.as_deref(),
            None,
        )?;
        if let Some(c) = &self.sc.setup.workspace_cwd {
            let dir = self.expand(c)?;
            self.create_context(Path::new(&dir))
                .map_err(StepErr::Harness)?;
            self.focus_client().map_err(StepErr::Harness)?;
            self.new_events();
        }
        Ok(())
    }

    fn registered(&self) -> Option<Value> {
        self.cli(&["plugin", "list"])
            .ok()?
            .get("plugins")?
            .as_array()?
            .iter()
            .find(|p| p["plugin_id"] == self.lp.plugin_id.as_str())
            .cloned()
    }

    /// The manifest's declaration of the pane entrypoint `pane`.
    fn pane_decl(&self, pane: &str) -> Option<&PaneDecl> {
        self.manifest.as_ref()?.panes.iter().find(|p| p.id == pane)
    }

    /// The placement the manifest declares for the pane entrypoint `pane`.
    fn declared_placement(&self, pane: &str) -> Option<String> {
        self.pane_decl(pane).map(|p| p.placement.clone())
    }

    /// The bare id of a scenario's action (scenarios may qualify it).
    fn action_id(&self, action: &str) -> String {
        self.manifest
            .as_ref()
            .and_then(|m| m.resolve_action(action))
            .map_or_else(|| action.to_string(), |a| a.id.clone())
    }
    // ---- lifecycle (18 §7.1, §8.3) ----------------------------------------------------------

    /// Install, then check the commit and manifest sha256 against the lock.
    fn install(&mut self) -> bool {
        let spec = self.lp.install_spec();
        let r = match self.cli(&["plugin", "install", &spec]) {
            Ok(r) => r,
            Err(e) => {
                // The mirror is trusted input: a fetch failure means the job is broken.
                let o = if Self::error_kind(&e) == "fetch_failed" || e.contains("fetch_failed") {
                    Outcome::HarnessError
                } else {
                    Outcome::SuspectedCompatFailure
                };
                self.lifecycle
                    .push(check("install", o, format!("{spec}: {e}")));
                return false;
            }
        };
        self.lifecycle.push(passed("install"));
        let root = PathBuf::from(r["plugin"]["root"].as_str().unwrap_or_default());
        let commit = r["plugin"]["origin"]["commit"].as_str().unwrap_or_default();
        let pinned = match registry::read_manifest(&root) {
            Ok((m, sha)) => {
                let ok = commit == self.lp.commit && sha == self.lp.manifest_sha256;
                self.manifest = Some(m);
                if ok {
                    passed("pinned_commit_and_manifest")
                } else {
                    check(
                        "pinned_commit_and_manifest",
                        Outcome::SuspectedCompatFailure,
                        format!(
                            "installed commit {commit} and manifest {sha}; the lock says {} and {}",
                            self.lp.commit, self.lp.manifest_sha256
                        ),
                    )
                }
            }
            Err(e) => check(
                "pinned_commit_and_manifest",
                Outcome::SuspectedCompatFailure,
                format!("{}: {e}", root.display()),
            ),
        };
        self.lifecycle.push(pinned);
        self.plugin_root = Some(root);
        // The registry's dirs must be the ones `{state}` and `{config}` already name.
        if let Some(p) = self.registered() {
            for (key, known) in [
                ("state_dir", &self.state_dir),
                ("config_dir", &self.config_dir),
            ] {
                let reported = p[key].as_str().map(PathBuf::from);
                let same = match (&reported, known) {
                    (Some(a), Some(b)) => {
                        a.canonicalize().unwrap_or(a.clone())
                            == b.canonicalize().unwrap_or(b.clone())
                    }
                    _ => false,
                };
                if !same {
                    self.lifecycle.push(check(
                        "plugin_dirs",
                        Outcome::HarnessError,
                        format!("{key} is {reported:?}, the harness expected {known:?}"),
                    ));
                }
            }
        }
        true
    }

    /// `plugin trust <id> --legacy`, which also builds. Returns whether the plugin is active.
    fn trust(&mut self, prefix: &str) -> Result<(), (Outcome, String)> {
        let id = self.lp.plugin_id.clone();
        match self.cli(&["plugin", "trust", &id, "--legacy"]) {
            Ok(_) => Ok(()),
            Err(e) if Self::error_kind(&e) == "build_failed" => Err((
                Outcome::DependencyFailure,
                format!("{prefix}build failed: {e}"),
            )),
            Err(e) => Err((
                Outcome::SuspectedCompatFailure,
                format!("{prefix}trust: {e}"),
            )),
        }
    }

    fn trust_and_build(&mut self) {
        let has_build = self
            .manifest
            .as_ref()
            .is_some_and(|m| !m.build_on(self.os).is_empty());
        let mut res = self.trust("");
        // A build that hit GitHub's limit for unauthenticated requests is tried again later.
        for pause in RATE_LIMIT_RETRIES {
            match &res {
                Err((Outcome::DependencyFailure, why)) if rate_limited(why) => {
                    eprintln!("note: the build looks rate limited; trying again in {pause:?}");
                    std::thread::sleep(pause);
                    res = self.trust("");
                }
                _ => break,
            }
        }
        match res {
            Ok(()) => {
                self.lifecycle.push(passed("trust"));
                let built = self.registered().is_some_and(|p| p["built"] == true);
                self.lifecycle.push(match (has_build, built) {
                    (false, _) => check(
                        "build",
                        Outcome::NotExercised,
                        "the manifest declares no build step for this platform".to_string(),
                    ),
                    (true, true) => passed("build"),
                    (true, false) => check(
                        "build",
                        Outcome::SuspectedCompatFailure,
                        "trust succeeded but the plugin is not marked built".to_string(),
                    ),
                });
            }
            Err((Outcome::DependencyFailure, why)) => {
                // The grant was made; the build revoked it again.
                self.lifecycle.push(passed("trust"));
                self.lifecycle
                    .push(check("build", Outcome::DependencyFailure, why.clone()));
                self.build_failed = Some(why);
            }
            Err((o, why)) => {
                self.lifecycle.push(check("trust", o, why.clone()));
                self.lifecycle.push(check(
                    "build",
                    Outcome::NotExercised,
                    "trust failed".to_string(),
                ));
                self.build_failed = Some(why);
            }
        }
    }

    /// The first activation, then "enabling does not run startup commands".
    fn activate(&mut self) {
        self.startup_base = self.startup_logs().len();
        self.restart();
        let n = self.startup_count();
        let timeout = self.step_timeout(None);
        if n > 0 && !self.wait_startup(timeout) {
            self.lifecycle.push(check(
                "startup_runs_on_activation",
                Outcome::SuspectedCompatFailure,
                format!("expected {n} startup invocation(s) after activation"),
            ));
        }
        if n == 0 {
            self.lifecycle.push(check(
                "enable_does_not_run_startup",
                Outcome::NotExercised,
                "the manifest declares no startup command".to_string(),
            ));
            return;
        }
        let before = self.startup_logs().len();
        let id = self.lp.plugin_id.clone();
        let toggled = self
            .cli(&["plugin", "disable", &id])
            .and_then(|_| self.cli(&["plugin", "enable", &id]));
        self.lifecycle.push(match toggled {
            Err(e) => check(
                "enable_does_not_run_startup",
                Outcome::SuspectedCompatFailure,
                format!("disable/enable: {e}"),
            ),
            Ok(_) => {
                // A startup run would show up within a couple of seconds of the change.
                let extra = poll(Duration::from_secs(3), || {
                    self.startup_logs().len() > before
                });
                if extra {
                    check(
                        "enable_does_not_run_startup",
                        Outcome::SuspectedCompatFailure,
                        "enabling the plugin ran its startup commands".to_string(),
                    )
                } else {
                    passed("enable_does_not_run_startup")
                }
            }
        });
    }

    /// After the steps: startup ran before dependent actions, and once per activation.
    fn startup_checks(&mut self) {
        let n = self.startup_count();
        if n == 0 {
            for c in ["startup_before_actions", "startup_once_per_activation"] {
                self.lifecycle.push(check(
                    c,
                    Outcome::NotExercised,
                    "the manifest declares no startup command".to_string(),
                ));
            }
            return;
        }
        let logs = self.logs();
        let started = |l: &Value| l["started_unix_ms"].as_i64().unwrap_or(i64::MAX);
        let first_startup = logs
            .iter()
            .filter(|l| l["source"] == "startup")
            .map(started)
            .min();
        let first_other = logs
            .iter()
            .filter(|l| l["source"] != "startup")
            .map(started)
            .min();
        self.lifecycle.push(match (first_startup, first_other) {
            (None, _) => check(
                "startup_before_actions",
                Outcome::SuspectedCompatFailure,
                "no startup invocation was recorded".to_string(),
            ),
            (Some(_), None) => check(
                "startup_before_actions",
                Outcome::NotExercised,
                "no other invocation ran".to_string(),
            ),
            (Some(a), Some(b)) if a <= b => passed("startup_before_actions"),
            (Some(_), Some(_)) => check(
                "startup_before_actions",
                Outcome::SuspectedCompatFailure,
                "an invocation started before the startup commands".to_string(),
            ),
        });
        // One more activation, then the count must be exactly one run per activation.
        self.restart();
        let timeout = self.step_timeout(None);
        self.wait_startup(timeout);
        let want = self.startup_base + n * self.activations as usize;
        // Late duplicates show up within a couple of seconds.
        poll(Duration::from_secs(3), || self.startup_logs().len() > want);
        let got = self.startup_logs().len();
        self.lifecycle.push(if got == want {
            passed("startup_once_per_activation")
        } else {
            check(
                "startup_once_per_activation",
                Outcome::SuspectedCompatFailure,
                format!(
                    "{got} startup invocation(s) after {} activation(s) of {n} command(s)",
                    self.activations
                ),
            )
        });
    }

    fn reinstall_keeps_config_and_state(&mut self) {
        const NAME: &str = "reinstall_keeps_config_and_state";
        let (Some(state), Some(config)) = (self.state_dir.clone(), self.config_dir.clone()) else {
            self.lifecycle.push(check(
                NAME,
                Outcome::NotExercised,
                "the plugin's config and state dirs are unknown".to_string(),
            ));
            return;
        };
        let marker = ".vibeke-harness-marker";
        write(&state.join(marker), "kept\n");
        write(&config.join(marker), "kept\n");
        let names = |d: &Path| -> BTreeSet<String> {
            std::fs::read_dir(d)
                .map(|r| {
                    r.flatten()
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default()
        };
        let (s0, c0) = (names(&state), names(&config));
        let spec = self.lp.install_spec();
        if let Err(e) = self.cli(&["plugin", "install", &spec]) {
            self.lifecycle.push(check(
                NAME,
                Outcome::SuspectedCompatFailure,
                format!("reinstall: {e}"),
            ));
            return;
        }
        if let Some(p) = self.registered() {
            self.plugin_root = p["root"].as_str().map(PathBuf::from);
        }
        if let Err((o, why)) = self.trust("reinstall: ") {
            self.lifecycle.push(check(NAME, o, why));
            return;
        }
        let lost: Vec<String> = s0
            .difference(&names(&state))
            .map(|n| format!("state/{n}"))
            .chain(
                c0.difference(&names(&config))
                    .map(|n| format!("config/{n}")),
            )
            .collect();
        self.lifecycle.push(if lost.is_empty() {
            passed(NAME)
        } else {
            check(
                NAME,
                Outcome::SuspectedCompatFailure,
                format!("lost after reinstall: {}", lost.join(", ")),
            )
        });
    }

    /// Disable (`disable`) or revoke (`untrust`) the plugin, then check that its callbacks are
    /// rejected: a declared action is refused, and every broker call its still-running
    /// processes (startup commands, panes) make afterwards fails.
    fn callbacks_rejected(&mut self, name: &str, op: &str) {
        let id = self.lp.plugin_id.clone();
        let from = match self.trace_since(self.trace_off) {
            Ok((_, off)) => off,
            Err(_) => self.trace_off,
        };
        let t = now_ms();
        if let Err(e) = self.cli(&["plugin", op, &id]) {
            self.lifecycle.push(check(
                name,
                Outcome::SuspectedCompatFailure,
                format!("plugin {op}: {e}"),
            ));
            return;
        }
        let mut evidence = Vec::new();
        let mut violations = Vec::new();
        let action = self
            .manifest
            .as_ref()
            .and_then(|m| m.actions_on(self.os).first().map(|a| a.id.clone()));
        if let Some(a) = action {
            match self.api(
                "plugin.action.run",
                json!({"plugin": id, "action": a, "context": {"workspace_id": self.ctx.workspace}}),
            ) {
                Err(_) => evidence.push(format!("action {a} refused")),
                Ok(r) => violations.push(format!("action {a} launched: {}", r["log"]["log_id"])),
            }
        }
        let mut lines = Vec::new();
        poll(Duration::from_secs(5), || {
            lines = self
                .trace_since(from)
                .map(|(l, _)| l)
                .unwrap_or_default()
                .into_iter()
                .filter(|l| l.ts_ms >= t && l.transport != "launcher")
                .collect();
            !lines.is_empty()
        });
        for l in &lines {
            if l.ok {
                violations.push(format!("{} {} succeeded after {op}", l.transport, l.method));
            } else {
                evidence.push(format!("{} rejected ({:?})", l.method, l.code));
            }
        }
        self.lifecycle.push(if !violations.is_empty() {
            check(name, Outcome::SuspectedCompatFailure, violations.join("; "))
        } else if evidence.is_empty() {
            check(
                name,
                Outcome::NotExercised,
                "the plugin has no action and made no callback afterwards".to_string(),
            )
        } else {
            check(name, Outcome::Passed, Some(evidence.join("; ")))
        });
    }

    fn uninstall(&mut self) {
        let id = self.lp.plugin_id.clone();
        let root = self.plugin_root.clone();
        self.lifecycle
            .push(match self.cli(&["plugin", "uninstall", &id]) {
                Err(e) => check("uninstall", Outcome::SuspectedCompatFailure, e),
                Ok(_) => {
                    let left = root.as_ref().is_some_and(|r| r.exists());
                    let listed = self.registered().is_some();
                    if left || listed {
                        check(
                            "uninstall",
                            Outcome::SuspectedCompatFailure,
                            format!("managed files left: {left}; still registered: {listed}"),
                        )
                    } else {
                        passed("uninstall")
                    }
                }
            });
    }

    /// Link the mirror checkout, then unlink it. Linking runs no plugin code (not trusted).
    fn link_unlink(&mut self) {
        let dir = match self.clone_dir() {
            Ok(d) => d,
            Err(e) => {
                self.lifecycle
                    .push(check("link_unlink", Outcome::HarnessError, e));
                return;
            }
        };
        let id = self.lp.plugin_id.clone();
        let r = self
            .cli(&["plugin", "link", dir.to_str().unwrap()])
            .and_then(|_| {
                if self.registered().is_none() {
                    return Err("not registered after link".into());
                }
                self.cli(&["plugin", "unlink", &id])
            })
            .and_then(|_| match self.registered() {
                Some(_) => Err("still registered after unlink".into()),
                None => Ok(()),
            });
        self.lifecycle.push(match r {
            Ok(()) => passed("link_unlink"),
            Err(e) => check("link_unlink", Outcome::SuspectedCompatFailure, e),
        });
    }

    // ---- the Vibeke side --------------------------------------------------------------------

    /// Every step on Vibeke, with the lifecycle checks around them; then every process the
    /// Vibeke side started is stopped (18 §11), so nothing of it is left when Herdr starts.
    fn vibeke_phase(&mut self, steps: &[&Step]) -> Vec<SideStep> {
        let not_run =
            |why: &str| -> Vec<SideStep> { steps.iter().map(|_| SideStep::not_run(why)).collect() };
        let out: Vec<SideStep> = if let Err(e) = self.context() {
            self.lifecycle
                .push(check("session", Outcome::HarnessError, e));
            not_run("not run: the Vibeke session could not be set up")
        } else if !self.install() {
            not_run("not run: the install failed")
        } else {
            match self.setup_files() {
                Err(StepErr::Harness(e) | StepErr::Fail(e)) => {
                    self.lifecycle
                        .push(check("setup", Outcome::HarnessError, e));
                    not_run("not run: the scenario setup failed")
                }
                Ok(()) => {
                    self.trust_and_build();
                    if self.build_failed.is_none() {
                        self.activate();
                    }
                    let out: Vec<SideStep> = steps.iter().map(|s| self.vibeke_step(s)).collect();
                    // The command logs as the steps left them (uninstall may drop them).
                    self.save_vibeke_plugin_logs();
                    self.after_steps();
                    out
                }
            }
        };
        self.save_logs();
        let stopped = self.stop_vibeke_side();
        self.lifecycle.push(stopped);
        out
    }

    /// The lifecycle checks after the steps (18 §8.3).
    fn after_steps(&mut self) {
        if self.build_failed.is_none() && self.registered().is_some() {
            self.startup_checks();
            self.reinstall_keeps_config_and_state();
            self.callbacks_rejected("disabled_callbacks_rejected", "disable");
            let _ = self.cli(&["plugin", "enable", self.id()]);
            self.callbacks_rejected("revoked_callbacks_rejected", "untrust");
        } else {
            let why = self
                .build_failed
                .clone()
                .unwrap_or_else(|| "the scenario removed the plugin".into());
            for c in [
                "startup_before_actions",
                "startup_once_per_activation",
                "reinstall_keeps_config_and_state",
                "disabled_callbacks_rejected",
                "revoked_callbacks_rejected",
            ] {
                self.lifecycle
                    .push(check(c, Outcome::NotExercised, why.clone()));
            }
        }
        if self.registered().is_some() {
            self.uninstall();
        } else {
            self.lifecycle.push(check(
                "uninstall",
                Outcome::NotExercised,
                "the plugin is no longer registered".to_string(),
            ));
        }
        self.link_unlink();
    }

    /// One step on Vibeke: the step, its expectations, and the grids of the panes it names.
    fn vibeke_step(&mut self, step: &Step) -> SideStep {
        let t0 = Instant::now();
        let deadline = t0 + self.step_timeout(Some(step));
        let mut ev = Vec::new();
        self.new_events();
        let seq0 = self.seq;
        let trace0 = match self.trace_since(self.trace_off) {
            Ok((_, off)) => off,
            Err(_) => self.trace_off,
        };
        self.trace_off = trace0;
        let logs0: BTreeSet<String> = self
            .logs()
            .iter()
            .filter_map(|l| l["log_id"].as_str().map(str::to_string))
            .collect();
        let (result, reason) = if let Some(b) = &self.build_failed {
            (SideResult::NotRun, Some(format!("not run: {b}")))
        } else {
            let r = self
                .do_step(step, &mut ev)
                .and_then(|_| self.expectations(step, seq0, trace0, &logs0, deadline, &mut ev));
            match r {
                Ok(()) => (SideResult::Passed, None),
                Err(StepErr::Fail(w)) => (SideResult::Failed, Some(w)),
                Err(StepErr::Harness(w)) => (SideResult::HarnessError, Some(w)),
            }
        };
        let mut side = SideStep {
            result,
            reason,
            evidence: ev,
            compare_why: vec![],
            captures: BTreeMap::new(),
            duration_ms: 0,
        };
        if side.result != SideResult::NotRun {
            // Compared panes, judged only when the step passed (a failed step's screen is
            // never a reference); then every other pane the step names, for the screenshots
            // (18 §10.5) and, when the step failed, to show what was on screen (H12).
            let judge = side.result == SideResult::Passed;
            for c in &step.compare {
                if let Some(cap) = self.capture_vibeke(
                    step,
                    &c.pane,
                    Some(c),
                    judge,
                    deadline,
                    &mut side.evidence,
                    &mut side.compare_why,
                ) {
                    side.captures.insert(c.pane.clone(), cap);
                }
            }
            for name in shot_panes(step) {
                if side.captures.contains_key(&name) {
                    continue;
                }
                let mut ignore = Vec::new();
                if let Some(cap) = self.capture_vibeke(
                    step,
                    &name,
                    None,
                    false,
                    deadline,
                    &mut side.evidence,
                    &mut ignore,
                ) {
                    side.captures.insert(name, cap);
                }
            }
        }
        if side.result == SideResult::Failed
            && let Some(t) = self.client_screen()
        {
            let f = self
                .logs_dir()
                .join(format!("{}-vibeke-client-screen.txt", safe_part(&step.id)));
            let _ = logs::write_redacted(&f, &t, logs::LOG_MAX_BYTES);
        }
        side.duration_ms = t0.elapsed().as_millis() as u64;
        side
    }

    fn manifest(&self) -> StepRes<&Manifest> {
        match &self.manifest {
            Some(m) => Ok(m),
            None => harness("the installed manifest is unknown"),
        }
    }

    /// Run one step on Vibeke. Evidence of the call itself goes into `ev`.
    fn do_step(&mut self, step: &Step, ev: &mut Vec<String>) -> StepRes<()> {
        let id = self.lp.plugin_id.clone();
        match step.kind {
            StepKind::Action => {
                let Some(action) = &step.action else {
                    return harness("action step without `action`");
                };
                // The socket call Herdr's own clients use, with the same context as on
                // Herdr (18 §11).
                let c = &self.ctx;
                let context = invocation_context(
                    step.context.as_deref(),
                    &c.workspace,
                    &c.tab,
                    &c.pane,
                    c.agent_pane.as_deref(),
                );
                let r = self
                    .compat_call(
                        "plugin.action.invoke",
                        json!({"plugin_id": id, "action_id": self.action_id(action), "context": context}),
                    )
                    .or_else(fail)?;
                let log = r
                    .pointer("/log/log_id")
                    .or(r.pointer("/log_id"))
                    .cloned()
                    .unwrap_or(Value::Null);
                ev.push(format!("action {action} launched ({log})"));
            }
            StepKind::Event => {
                if !step.cli.is_empty() {
                    let args = step
                        .cli
                        .iter()
                        .map(|a| self.expand(a))
                        .collect::<StepRes<Vec<_>>>()?;
                    self.herdr_cli(&args).or_else(fail)?;
                    ev.push(format!("herdr {}", args.join(" ")));
                } else if let Some(st) = &step.agent_status {
                    let Some(p) = self.ctx.agent_pane.clone() else {
                        return harness("agent_status needs `setup.fake_agent = true`");
                    };
                    self.herdr_cli(&[
                        "pane".into(),
                        "report-agent".into(),
                        p,
                        "--agent".into(),
                        "claude".into(),
                        "--state".into(),
                        st.clone(),
                    ])
                    .or_else(fail)?;
                    ev.push(format!("fake agent reported {st}"));
                } else {
                    return harness("event step without `cli` or `agent_status`");
                }
            }
            StepKind::Link => {
                let Some(url) = &step.url else {
                    return harness("link step without `url`");
                };
                let url = self.expand(url)?;
                let handlers: Vec<String> = {
                    let m = self.manifest()?;
                    m.link_handlers
                        .iter()
                        .filter(|l| m.entry_on(l.platforms.as_ref(), self.os))
                        .map(|l| l.id.clone())
                        .collect()
                };
                if handlers.is_empty() {
                    return harness("the plugin declares no link handler on this platform");
                }
                // The server matches the URL against each handler's pattern.
                let mut errors = Vec::new();
                let c = &self.ctx;
                let context =
                    json!({"workspace_id": c.workspace, "tab_id": c.tab, "pane_id": c.pane});
                for h in &handlers {
                    match self.api(
                        "plugin.link.open",
                        json!({"plugin": id, "handler": h, "url": url, "context": context}),
                    ) {
                        Ok(r) => {
                            ev.push(format!("link handler {h} opened ({})", r["log"]["log_id"]));
                            return Ok(());
                        }
                        Err(e) => errors.push(format!("{h}: {e}")),
                    }
                }
                return fail(format!("no link handler took {url}: {}", errors.join("; ")));
            }
            StepKind::Restart => {
                self.restart();
                ev.push(format!(
                    "server restarted (activation {})",
                    self.activations
                ));
            }
            StepKind::Lifecycle => {
                let Some(op) = step.lifecycle else {
                    return harness("lifecycle step without `lifecycle`");
                };
                let spec = self.lp.install_spec();
                let link_dir = if op == LifecycleOp::Link {
                    Some(self.clone_dir().map_err(StepErr::Harness)?)
                } else {
                    None
                };
                let args: Vec<String> = match op {
                    LifecycleOp::Install | LifecycleOp::Reinstall => {
                        vec!["plugin".into(), "install".into(), spec]
                    }
                    LifecycleOp::Trust | LifecycleOp::Build => {
                        vec![
                            "plugin".into(),
                            "trust".into(),
                            id.clone(),
                            "--legacy".into(),
                        ]
                    }
                    LifecycleOp::Link => vec![
                        "plugin".into(),
                        "link".into(),
                        link_dir.unwrap().to_string_lossy().into_owned(),
                    ],
                    LifecycleOp::Unlink => vec!["plugin".into(), "unlink".into(), id.clone()],
                    LifecycleOp::Enable => vec!["plugin".into(), "enable".into(), id.clone()],
                    LifecycleOp::Disable => vec!["plugin".into(), "disable".into(), id.clone()],
                    LifecycleOp::Revoke => vec!["plugin".into(), "untrust".into(), id.clone()],
                    LifecycleOp::Uninstall => {
                        vec!["plugin".into(), "uninstall".into(), id.clone()]
                    }
                };
                let a: Vec<&str> = args.iter().map(String::as_str).collect();
                self.cli(&a).or_else(fail)?;
                if op == LifecycleOp::Reinstall {
                    self.cli(&["plugin", "trust", &id, "--legacy"])
                        .or_else(fail)?;
                }
                ev.push(format!("vibeke {}", args.join(" ")));
            }
            StepKind::Pane => {
                let Some(pane) = &step.pane else {
                    return harness("pane step without `pane`");
                };
                // Herdr's rule (as `herdr plugin pane open`): split and zoomed panes target
                // the context pane, the others the host's focus.
                let mut params = json!({"plugin_id": id, "entrypoint": pane});
                if matches!(
                    self.declared_placement(pane).as_deref(),
                    Some("split" | "zoomed")
                ) {
                    params["target_pane_id"] = json!(self.ctx.pane);
                }
                let r = self.compat_call("plugin.pane.open", params).or_else(fail)?;
                let placement = r
                    .pointer("/plugin_pane/placement")
                    .or(r.pointer("/placement"))
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                ev.push(format!("pane {pane} opened ({placement})"));
            }
            StepKind::Input => {
                let Some(pane) = &step.pane else {
                    return harness("input step without `pane`");
                };
                let Some(target) = self.plugin_pane(pane) else {
                    return fail(format!("plugin pane {pane} is not open"));
                };
                if let Some(t) = &step.text {
                    let t = self.expand(t)?;
                    self.api("pane.send_text", json!({"pane": target, "text": t}))
                        .or_else(fail)?;
                    ev.push(format!("sent text to {pane}"));
                }
                if !step.keys.is_empty() {
                    let keys: Vec<String> = step.keys.iter().map(|k| vibeke_key(k)).collect();
                    self.api("pane.send_keys", json!({"pane": target, "keys": keys}))
                        .or_else(fail)?;
                    ev.push(format!("sent keys {:?} to {pane}", step.keys));
                }
            }
            StepKind::Wait => {}
        }
        Ok(())
    }

    /// The Vibeke pane of the plugin pane entrypoint `name`: the open popup, or the most
    /// recently opened plugin pane with that entrypoint.
    fn plugin_pane(&self, name: &str) -> Option<String> {
        self.plugin_pane_placed(name).map(|(p, _)| p)
    }

    fn plugin_pane_placed(&self, name: &str) -> Option<(String, String)> {
        if let Ok(ui) = self.api("compat.ui.state", json!({}))
            && ui["popup"]["plugin_id"] == self.lp.plugin_id.as_str()
            && ui["popup"]["entrypoint_id"] == name
            && let Some(p) = ui["popup"]["pane"].as_str()
        {
            return Some((p.to_string(), "popup".into()));
        }
        self.events_since(0, "plugin.pane_opened")
            .into_iter()
            .rev()
            .find(|e| {
                e["actor"]["id"] == self.lp.plugin_id.as_str() && e["data"]["entrypoint_id"] == name
            })
            .and_then(|e| {
                Some((
                    e["subject"]["pane"].as_str()?.to_string(),
                    e["data"]["placement"].as_str().unwrap_or("").to_string(),
                ))
            })
    }

    fn screen_text(&self, pane: &str) -> Option<String> {
        let r = self
            .api(
                "pane.screenshot",
                json!({"pane": pane, "format": "text", "inline": true}),
            )
            .ok()?;
        r["data"].as_str().map(str::to_string)
    }

    /// One look at an expectation: `Ok(Some(evidence))` when it holds, `Ok(None)` when not yet.
    fn look(
        &self,
        e: &Expectation,
        seq0: i64,
        logs0: &BTreeSet<String>,
        action: Option<&str>,
    ) -> StepRes<Option<String>> {
        let id = self.lp.plugin_id.as_str();
        Ok(match e {
            Expectation::InvocationFinal { status } => {
                let want = match status.as_deref().unwrap_or("completed") {
                    "completed" | "succeeded" => "succeeded",
                    "failed" => "failed",
                    other => return harness(format!("unknown invocation status `{other}`")),
                };
                let new: Vec<Value> = self
                    .logs()
                    .into_iter()
                    .filter(|l| {
                        l["source"] != "startup"
                            && !logs0.contains(l["log_id"].as_str().unwrap_or_default())
                    })
                    .collect();
                match final_invocation(&new, want, action) {
                    Ok(l) => l.map(|l| format!("invocation {} {want}", l["log_id"])),
                    Err(why) => return fail(why),
                }
            }
            Expectation::Pane { pane, placement } => self
                .events_since(seq0, "plugin.pane_opened")
                .into_iter()
                .find(|ev| {
                    ev["actor"]["id"] == id
                        && ev["data"]["entrypoint_id"] == pane.as_str()
                        && placement
                            .as_deref()
                            .is_none_or(|p| ev["data"]["placement"] == p)
                })
                .map(|ev| format!("pane {pane} at {}", ev["data"]["placement"])),
            Expectation::Popup { pane } => {
                let ui = self.api("compat.ui.state", json!({})).unwrap_or_default();
                (ui["popup"]["plugin_id"] == id && ui["popup"]["entrypoint_id"] == pane.as_str())
                    .then(|| format!("popup {pane}"))
            }
            Expectation::ScreenText { pane, text, regex } => {
                let re = match regex {
                    Some(r) => {
                        Some(regex::Regex::new(r).map_err(|e| StepErr::Harness(e.to_string()))?)
                    }
                    None => None,
                };
                let Some(p) = self.plugin_pane(pane) else {
                    return Ok(None);
                };
                let screen = self.screen_text(&p).unwrap_or_default();
                let hit = text.as_deref().is_none_or(|t| screen.contains(t))
                    && re.as_ref().is_none_or(|r| r.is_match(&screen));
                hit.then(|| format!("screen text in {pane}"))
            }
            Expectation::File { path, contains } => {
                let p = self.expand(path)?;
                let p = self.home().join(p);
                let ok = p.exists() && contains.as_deref().is_none_or(|c| read(&p).contains(c));
                ok.then(|| format!("file {}", p.display()))
            }
            Expectation::HookLog { text } => self
                .logs()
                .into_iter()
                .find(|l| {
                    !l["event"].is_null()
                        && format!("{}{}", l["stdout"], l["stderr"]).contains(text.as_str())
                })
                .map(|l| format!("event hook {} logged the text", l["event"])),
            Expectation::Ready { text } => self
                .startup_logs()
                .into_iter()
                .find(|l| format!("{}{}", l["stdout"], l["stderr"]).contains(text.as_str()))
                .map(|l| format!("startup {} ready", l["entrypoint_id"])),
            Expectation::Event { event_type } => {
                // The server's own projection is the best evidence: a hook of this plugin
                // that the server ran for the event since the step started. (The harness
                // cannot project every event itself: `pane.agent_status_changed` needs the
                // server's agent model, which events do not carry.)
                let wire = vk_compat::herdr::events::wire_name(event_type);
                if let Some(l) = self.logs().into_iter().find(|l| {
                    !logs0.contains(l["log_id"].as_str().unwrap_or_default())
                        && (l["event"] == event_type.as_str() || l["event"] == wire.as_str())
                }) {
                    return Ok(Some(format!(
                        "event {event_type} (its hook ran: {})",
                        l["log_id"]
                    )));
                }
                // Otherwise a Vibeke event of that type, or one the compat projection turns
                // into it (scenarios name Herdr events, such as `worktree.created`).
                let evs = self
                    .api("events.read", json!({"after": seq0, "limit": 500}))
                    .ok()
                    .and_then(|v| v["events"].as_array().cloned())
                    .unwrap_or_default();
                let mut proj = vk_compat::herdr::events::Projector::new();
                evs.iter()
                    .find(|ev| {
                        let kind = ev["type"].as_str().unwrap_or_default();
                        let subj = |k: &str| ev["subject"][k].as_str();
                        let status = ev["data"]["state"]
                            .as_str()
                            .or(ev["data"]["status"].as_str());
                        let input = vk_compat::herdr::events::Input {
                            kind,
                            workspace: subj("workspace"),
                            tab: subj("tab"),
                            pane: subj("pane"),
                            status_after: status,
                            at_ms: 0,
                        };
                        kind == event_type.as_str()
                            || proj.project(&input).contains(&event_type.as_str())
                    })
                    .map(|ev| format!("event {event_type} (from {})", ev["type"]))
            }
            Expectation::StubRequest { stub, contains } => {
                let Some(log) = self.sinks.get(stub) else {
                    return harness(format!("no http_sink stub `{stub}`"));
                };
                let text = read(log);
                let hit = text
                    .lines()
                    .any(|l| contains.as_deref().is_none_or(|c| l.contains(c)));
                hit.then(|| format!("stub {stub} received a request"))
            }
        })
    }

    /// Wait for every expectation of `step` (and its expected calls) until `deadline`.
    fn expectations(
        &mut self,
        step: &Step,
        seq0: i64,
        trace0: u64,
        logs0: &BTreeSet<String>,
        deadline: Instant,
        ev: &mut Vec<String>,
    ) -> StepRes<()> {
        let mut pending: Vec<&Expectation> = step.expect.iter().collect();
        let mut calls_ok = step.expect_calls.is_empty();
        let mut mismatches;
        let mut unexpected;
        let mut off;
        loop {
            let mut still = Vec::new();
            for e in pending {
                match self.look(e, seq0, logs0, step.action.as_deref())? {
                    Some(x) => ev.push(x),
                    None => still.push(e),
                }
            }
            pending = still;
            let (lines, o) = self.trace_since(trace0)?;
            off = o;
            let (m, u) = trace::match_calls(&lines, &self.lp.plugin_id, &step.expect_calls);
            if !calls_ok && m.is_empty() {
                calls_ok = true;
                ev.push(format!("{} expected call(s) seen", step.expect_calls.len()));
            }
            mismatches = m;
            unexpected = u;
            if (pending.is_empty() && calls_ok) || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        self.trace_off = self.trace_off.max(off);
        for u in &unexpected {
            ev.push(describe_failed_call(u));
        }
        let mut why = Vec::new();
        if !pending.is_empty() {
            why.push(format!("no evidence for {pending:?}"));
        }
        if !calls_ok {
            why.push(format!("expected calls not seen: {mismatches:?}"));
        }
        if why.is_empty() {
            Ok(())
        } else {
            // What the step's invocations did, so a timeout says more than "no evidence".
            for l in new_invocations(self.logs(), logs0, "source").iter().take(5) {
                ev.push(invocation_details(l));
            }
            fail(why.join("; "))
        }
    }

    /// Resize a plugin pane toward `cols`×`rows`; returns whether it has exactly that size.
    fn resize_to(&self, pane: &str, cols: u16, rows: u16) -> bool {
        let size = || -> Option<(i64, i64)> {
            let p = self.s.pane(pane);
            Some((p["cols"].as_i64()?, p["rows"].as_i64()?))
        };
        for _ in 0..4 {
            let Some((c, r)) = size() else {
                return false;
            };
            if (c, r) == (cols as i64, rows as i64) {
                return true;
            }
            let dc = cols as i64 - c;
            let dr = rows as i64 - r;
            if dc != 0 {
                let dir = if dc > 0 { "right" } else { "left" };
                let _ = self.api(
                    "pane.resize",
                    json!({"pane": pane, "direction": dir, "cells": dc.abs()}),
                );
            }
            if dr != 0 {
                let dir = if dr > 0 { "down" } else { "up" };
                let _ = self.api(
                    "pane.resize",
                    json!({"pane": pane, "direction": dir, "cells": dr.abs()}),
                );
            }
        }
        size() == Some((cols as i64, rows as i64))
    }

    /// Capture the canonical grid of a Vibeke pane (18 §10.3).
    fn capture(&self, pane: &str) -> anyhow::Result<Grid> {
        let r = self
            .api(
                "pane.screenshot",
                json!({"pane": pane, "format": "grid", "inline": true}),
            )
            .map_err(anyhow::Error::msg)?;
        // `data` is the grid JSON as a string, like the other inline formats.
        let v: Value = match &r["data"] {
            Value::String(s) => serde_json::from_str(s)?,
            other => other.clone(),
        };
        let (mut g, changes) = Grid::from_vibeke_json(&v)?;
        g.resolve_palette(&changes);
        Ok(g)
    }

    /// Write a canonical grid file (compact JSON: a pretty-printed 120x40 grid is over 1 MiB,
    /// and every step may keep one or two).
    fn write_grid(&self, name: &str, g: &Grid) -> Option<String> {
        let _ = std::fs::create_dir_all(&self.grid_dir);
        let json = serde_json::to_string(g).ok()?;
        std::fs::write(self.grid_dir.join(name), json).ok()?;
        Some(format!("{}/{name}", self.grid_rel))
    }

    /// The outer rectangle of a Vibeke pane relative to its pane area, from the compatibility
    /// layer's `pane.layout` (Herdr's shape, so both sides are measured the same way).
    fn vibeke_rect(&self, pane: &str) -> Result<Rect, String> {
        let r = self.compat_call("pane.layout", json!({"pane_id": pane}))?;
        herdr::parse_layout(&r)
            .and_then(|l| l.rect_of(pane))
            .map_err(|e| format!("{e:#}"))
    }

    /// The Vibeke grid of the plugin pane `name` after a step (18 §10). With `compare`, the
    /// pane is first resized to the declared size (except placements whose size the host
    /// decides) and must become stable with the comparison's masks; with `judge`, the
    /// Vibeke-only checks (stability, reference snapshot, 18 §10.5) add to `why`. The
    /// cross-side comparison happens in [`Run::merge_step`].
    #[allow(clippy::too_many_arguments)]
    fn capture_vibeke(
        &mut self,
        step: &Step,
        name: &str,
        compare: Option<&Compare>,
        judge: bool,
        deadline: Instant,
        ev: &mut Vec<String>,
        why: &mut Vec<String>,
    ) -> Option<Capture> {
        let Some((pane, placement)) = self.plugin_pane_placed(name) else {
            if judge {
                why.push(format!("plugin pane {name} not found for comparison"));
            }
            return None;
        };
        if self.vibeke_grids >= MAX_GRIDS {
            return None;
        }
        let mut cap = Capture {
            placement: Some(placement.clone()),
            ..Capture::default()
        };
        let masks: &[mask::Mask] = compare.map(|c| c.masks.as_slice()).unwrap_or(&[]);
        if let Some(c) = compare
            && let Some(size) = c.size
            && !c.placement_only
            && !fixed_size(&placement)
            && !self.resize_to(&pane, size.cols, size.rows)
        {
            cap.notes.push(format!(
                "Vibeke could not resize {name} to {}x{}",
                size.cols, size.rows
            ));
        }
        let (stable_for, timeout) = match compare {
            Some(c) => (
                Duration::from_millis(c.stable_ms.unwrap_or(500)),
                remaining(deadline).max(Duration::from_secs(5)),
            ),
            None => (Duration::from_millis(300), Duration::from_secs(3)),
        };
        let (vg, stable) =
            match mask::wait_stable(|| self.capture(&pane), masks, stable_for, timeout) {
                Ok(x) => x,
                Err(e) => {
                    if judge {
                        why.push(format!("capture {name}: {e:#}"));
                    }
                    return None;
                }
            };
        if judge && !stable {
            why.push(format!("{name} never became stable"));
        }
        let vg = render::redact(&vg);
        let base = format!("{}__{name}", safe_part(&step.id));
        cap.file = self.write_grid(&format!("{base}.vibeke.json"), &vg);
        self.vibeke_grids += 1;
        if placement != "popup" {
            match self.vibeke_rect(&pane) {
                Ok(r) => cap.rect = Some(r),
                Err(e) => cap
                    .notes
                    .push(format!("Vibeke reports no layout for {name}: {e}")),
            }
        }
        // Reviewed reference snapshots of Vibeke's own grid (18 §10.5). A failed step's
        // screen is never a reference.
        if judge && let Some(c) = compare {
            let snap = plugins_dir().join(format!("snapshots/{}/{base}.json", self.stem));
            if std::env::var("VIBEKE_UPDATE_SNAPSHOTS").as_deref() == Ok("1") {
                write(
                    &snap,
                    &serde_json::to_string_pretty(&vg).unwrap_or_default(),
                );
                ev.push(format!("snapshot {} written", snap.display()));
            } else if snap.exists() {
                match serde_json::from_str::<Grid>(&read(&snap))
                    .map_err(anyhow::Error::from)
                    .and_then(|g| mask::apply(&g, &c.masks))
                {
                    Ok(reference) => {
                        let d = diff::compare(&reference, &vg);
                        if d.is_empty() {
                            ev.push(format!("{name} matches its reference snapshot"));
                        } else {
                            ev.push(format!(
                                "{name}: {} cell(s) differ from the reference snapshot",
                                d.cells.len()
                            ));
                            why.push(format!("{name} differs from its reference snapshot"));
                        }
                    }
                    Err(e) => why.push(format!("{}: {e:#}", snap.display())),
                }
            }
        }
        cap.grid = Some(vg);
        Some(cap)
    }

    /// Stop everything the Vibeke side started (18 §11, H9): the TUI client, the server and its
    /// pane holders, and every process whose environment or working directory is inside the
    /// Vibeke side's HOME or session directory (startup commands, actions, panes, and the
    /// daemons they left behind, also in their own sessions). Then wait until they are gone and
    /// the TCP ports they listened on are free again, so nothing answers the Herdr side.
    fn stop_vibeke_side(&mut self) -> CheckResult {
        const NAME: &str = "vibeke_side_stopped";
        self.detach_client();
        let roots: Vec<String> = [self.home(), self.s.path("")]
            .iter()
            .map(|p| p.display().to_string().trim_end_matches('/').to_string())
            .collect();
        let mut pids = processes_under(&roots);
        let server = self.s.server_pid();
        if server > 0 {
            pids.extend(
                vk_hold::procinfo::tree(server as u32, 8)
                    .into_iter()
                    .map(|p| p.pid),
            );
        }
        let ports = listening_ports(&pids);
        let _ = self.s.cmd(&["server", "stop", "--kill-panes"]).output();
        // Processes that started while the server stopped.
        pids.extend(processes_under(&roots));
        kill_processes(&pids);
        let mut left = Vec::new();
        let gone = poll(Duration::from_secs(15), || {
            let live = live_pids();
            left = pids
                .iter()
                .filter(|p| live.contains(*p))
                .map(|p| format!("pid {p}"))
                .chain(
                    ports
                        .iter()
                        .filter(|p| !port_free(**p))
                        .map(|p| format!("port {p}")),
                )
                .collect();
            if !left.is_empty() {
                kill_processes(&pids);
            }
            left.is_empty()
        });
        let summary = format!(
            "{} process(es) stopped; ports freed: {}",
            pids.len(),
            if ports.is_empty() {
                "none listened".to_string()
            } else {
                ports
                    .iter()
                    .map(u16::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        );
        if gone {
            check(NAME, Outcome::Passed, Some(summary))
        } else {
            check(
                NAME,
                Outcome::HarnessError,
                format!("still there after the stop: {}; {summary}", left.join(", ")),
            )
        }
    }

    // ---- the Herdr side (18 §11) ------------------------------------------------------------

    /// Every step on Herdr, after the Vibeke side has stopped. It runs whatever happened on
    /// Vibeke (also when the Vibeke install or build failed, H3): Herdr installs and builds the
    /// plugin itself.
    fn herdr_phase(&mut self, steps: &[&Step]) -> Vec<SideStep> {
        self.start_herdr();
        if self.herdr.is_none() {
            let note = self.herdr_note.clone();
            return steps.iter().map(|_| SideStep::not_run(&note)).collect();
        }
        let out: Vec<SideStep> = steps.iter().map(|s| self.herdr_step_full(s)).collect();
        if let Some(h) = self.herdr.take() {
            let _ = logs::write_redacted(
                &self.logs_dir().join("herdr-plugin-logs.jsonl"),
                &herdr_logs(&h, &self.lp.plugin_id)
                    .iter()
                    .map(Value::to_string)
                    .collect::<Vec<_>>()
                    .join("\n"),
                logs::LOG_MAX_BYTES,
            );
            self.save_herdr_logs(&h);
            let _ = h.stop();
        }
        out
    }

    fn start_herdr(&mut self) {
        let Some(bin) = std::env::var_os("VIBEKE_HERDR_BIN").map(PathBuf::from) else {
            return;
        };
        let Ok(sha) = std::env::var("VIBEKE_HERDR_SHA256") else {
            self.lifecycle.push(check(
                "herdr_binary",
                Outcome::HarnessError,
                "VIBEKE_HERDR_BIN needs VIBEKE_HERDR_SHA256".to_string(),
            ));
            return;
        };
        // The checksum is verified before the binary runs at all.
        if let Err(e) = herdr::verify_binary(&bin, &sha) {
            self.lifecycle.push(check(
                "herdr_binary",
                Outcome::HarnessError,
                format!("{e:#}"),
            ));
            return;
        }
        let stub_env = match &self.herdr_stubs {
            Ok(st) => st.env.clone(),
            Err(e) => {
                self.herdr_note = e.clone();
                return;
            }
        };
        let _ = std::fs::create_dir_all(&self.herdr_root);
        // The same allowlisted environment as the Vibeke side (18 §5.3), with Herdr's own
        // directory on PATH after the toolchains, then the user tool dirs of Herdr's HOME.
        // TERM stays Herdr's: its TUI runs in the harness's PTY.
        let bin_dir = bin
            .parent()
            .map(|d| d.display().to_string())
            .unwrap_or_default();
        let get = |n: &str| std::env::var(n).ok();
        let mut dirs = vec![bin_dir];
        dirs.extend(user_bin_dirs(&self.herdr_root.join("home"), get, self.os));
        let mut env = plugin_env(&env_allowlist(), get, &dirs);
        env.retain(|(k, _)| k != "TERM");
        env.extend(stub_env);
        without_path_dirs(&mut env, &self.sc.setup.path_exclude);
        let opts = HerdrOptions {
            bin,
            root: self.herdr_root.clone(),
            cols: TUI_COLS,
            rows: TUI_ROWS,
            trace: self.herdr_trace.clone(),
            env,
        };
        let mut side = match HerdrSide::start(opts) {
            Ok(h) => h,
            Err(e) => {
                self.lifecycle.push(check(
                    "herdr_start",
                    Outcome::HarnessError,
                    format!("{e:#}"),
                ));
                return;
            }
        };
        match self.prepare_herdr(&side) {
            Ok(()) => {
                self.subscribe_herdr_events(&mut side);
                self.herdr = Some(side);
                self.herdr_note.clear();
            }
            Err(e) => {
                self.herdr_note = clip(&e, TEXT_MAX);
                self.save_herdr_logs(&side);
                let _ = side.stop();
            }
        }
    }

    /// Subscribe to every Herdr event (H4): the global kinds and the fake agent pane's status.
    fn subscribe_herdr_events(&mut self, side: &mut HerdrSide) {
        let agent: Vec<&str> = self.herdr_agent_pane.as_deref().into_iter().collect();
        let subs = herdr::event_subscriptions(&agent);
        self.herdr_events_err = side
            .subscribe_events(subs, &self.herdr_events)
            .err()
            .map(|e| format!("events.subscribe failed: {e:#}"));
    }

    /// Herdr's context: the TUI's active workspace, tab and focused pane, and a fake agent pane
    /// split below the focused pane when the scenario asks for one.
    fn herdr_context(&mut self, side: &HerdrSide) -> Result<(), String> {
        let (workspace, tab, pane) = side
            .active()
            .map_err(|e| format!("Herdr side not set up: {e:#}"))?;
        self.herdr_agent_pane = None;
        if self.sc.setup.fake_agent {
            let r = side
                .call(
                    "pane.split",
                    json!({"target_pane_id": pane, "direction": "down", "focus": false}),
                )
                .map_err(|e| format!("Herdr fake agent pane: {e:#}"))?;
            let id = r
                .pointer("/pane/pane_id")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("Herdr fake agent pane: no pane id in {r}"))?;
            self.herdr_agent_pane = Some(id.to_string());
        }
        if let Ok(st) = self.herdr_stubs.as_mut() {
            for (k, v) in [("workspace", &workspace), ("tab", &tab), ("pane", &pane)] {
                st.vars.values.insert(k.into(), v.clone());
            }
            if let Some(a) = &self.herdr_agent_pane {
                st.vars.values.insert("agent_pane".into(), a.clone());
            }
        }
        self.herdr_workspace = Some(workspace);
        self.herdr_tab = Some(tab);
        self.herdr_pane = Some(pane);
        Ok(())
    }

    /// The Herdr side's context and `[setup]` (with `setup.workspace_cwd`, a new workspace in
    /// that directory becomes the context), then the plugin installed the way a Herdr user
    /// installs it: `herdr plugin install <owner/repo> --ref <commit> --yes`, which previews,
    /// runs the manifest's build commands and registers the plugin. Git fetches the plugin from
    /// the local mirror ([`herdr::git_mirror_env`]); the installed commit and manifest are
    /// checked against the lock. A build that hit GitHub's request limit is tried again.
    fn prepare_herdr(&mut self, side: &HerdrSide) -> Result<(), String> {
        self.herdr_context(side)?;
        let id = self.lp.plugin_id.clone();
        let home = side.home().to_path_buf();
        let config = self
            .herdr_root
            .join("config/herdr/plugins/config")
            .join(&id);
        let herdr_config = herdr_config_file(&self.herdr_root);
        let cwd = {
            let Ok(st) = self.herdr_stubs.as_mut() else {
                return Err("Herdr side not set up: no placeholder values".into());
            };
            match materialize_setup(
                self.sc,
                &id,
                &home,
                &mut st.vars,
                Some(&config),
                Some(&herdr_config),
            ) {
                Ok(()) => {}
                Err(StepErr::Harness(e) | StepErr::Fail(e)) => {
                    return Err(format!("Herdr side setup: {e}"));
                }
            }
            match &self.sc.setup.workspace_cwd {
                Some(c) => Some(
                    scenario::expand(c, &st.vars)
                        .map_err(|e| format!("Herdr side setup: {e:#}"))?,
                ),
                None => None,
            }
        };
        if let Some(cwd) = cwd {
            side.call("workspace.create", json!({"cwd": cwd, "focus": true}))
                .map_err(|e| format!("Herdr workspace in {cwd}: {e:#}"))?;
            self.herdr_context(side)?;
        }
        let args = herdr_install_args(&self.lp.repository, self.lp.subdir(), &self.lp.commit);
        let a: Vec<&str> = args.iter().map(String::as_str).collect();
        let git = herdr::git_mirror_env(
            &self.gate.mirror.join(&self.lp.repository),
            &self.lp.repository,
        );
        let mut pauses = RATE_LIMIT_RETRIES.iter();
        loop {
            match side.cli_with(&a, &git, HERDR_INSTALL_TIMEOUT) {
                Ok((0, _, _)) => break,
                Ok((code, out, err)) => {
                    if rate_limited(&format!("{out}{err}"))
                        && let Some(pause) = pauses.next()
                    {
                        eprintln!(
                            "note: the Herdr install looks rate limited; trying again in {pause:?}"
                        );
                        std::thread::sleep(*pause);
                        continue;
                    }
                    return Err(format!(
                        "Herdr could not install the plugin (herdr {} exited {code}): {}{}",
                        args.join(" "),
                        err.trim(),
                        if out.trim().is_empty() {
                            String::new()
                        } else {
                            format!("; stdout: {}", out.trim())
                        }
                    ));
                }
                Err(e) => return Err(format!("Herdr could not install the plugin: {e:#}")),
            }
        }
        // The installed commit and manifest must be the locked ones.
        let listed = side
            .call("plugin.list", json!({"plugin_id": id}))
            .map_err(|e| format!("Herdr plugin.list after install: {e:#}"))?;
        let p = listed["plugins"]
            .as_array()
            .and_then(|a| a.iter().find(|p| p["plugin_id"] == id.as_str()))
            .ok_or_else(|| format!("Herdr does not list {id} after install: {listed}"))?;
        let commit = p.pointer("/source/resolved_commit").and_then(Value::as_str);
        let manifest = p["manifest_path"].as_str().unwrap_or_default();
        let bytes = std::fs::read(manifest).unwrap_or_default();
        let sha = registry::sha256_hex(&bytes);
        if commit != Some(self.lp.commit.as_str()) || sha != self.lp.manifest_sha256 {
            return Err(format!(
                "Herdr installed commit {commit:?} with manifest sha256 {sha:?}; the lock says {} and {}",
                self.lp.commit, self.lp.manifest_sha256
            ));
        }
        // When the Vibeke side could not install the plugin, the steps still need the
        // manifest: it is the same file.
        if self.manifest.is_none() {
            self.manifest = std::str::from_utf8(&bytes)
                .ok()
                .and_then(|t| Manifest::parse(t).ok());
        }
        Ok(())
    }

    /// One step on Herdr: the step, its expectations, and the grids of the panes it names.
    fn herdr_step_full(&mut self, step: &Step) -> SideStep {
        let t0 = Instant::now();
        let mut ev = Vec::new();
        self.herdr_off = trace::read(&self.herdr_trace, self.herdr_off)
            .map(|(_, o)| o)
            .unwrap_or(self.herdr_off);
        let step_from = self.herdr_off;
        self.herdr_events_off = std::fs::metadata(&self.herdr_events)
            .map(|m| m.len())
            .unwrap_or(0);
        // The panes before the step, so the panes it opens can be told apart (H2).
        self.herdr_before = self.herdr.as_ref().map(herdr_pane_ids).unwrap_or_default();
        let (result, reason) = if step.kind == StepKind::Restart {
            self.herdr_restart(step, &mut ev)
        } else {
            self.herdr_step(step, &mut ev)
        };
        let mut side = SideStep {
            result,
            reason,
            evidence: ev,
            compare_why: vec![],
            captures: BTreeMap::new(),
            duration_ms: 0,
        };
        let Some(h) = self.herdr.take() else {
            side.duration_ms = t0.elapsed().as_millis() as u64;
            return side;
        };
        // Which popup this step opened (or failed to open), from Herdr's trace (H7).
        let lines = trace::read(&self.herdr_trace, step_from)
            .map(|(l, _)| l)
            .unwrap_or_default();
        self.update_herdr_popup(&lines);
        if matches!(side.result, SideResult::Passed | SideResult::Failed) {
            for c in &step.compare {
                if let Some(cap) = self.capture_herdr(&h, step, &c.pane, Some(c), &lines) {
                    side.captures.insert(c.pane.clone(), cap);
                }
            }
            for name in shot_panes(step) {
                if side.captures.contains_key(&name) {
                    continue;
                }
                if let Some(cap) = self.capture_herdr(&h, step, &name, None, &lines) {
                    side.captures.insert(name, cap);
                }
            }
        }
        if side.result == SideResult::Failed
            && let Ok(t) = h.screen_text()
        {
            let f = self
                .logs_dir()
                .join(format!("{}-herdr-client-screen.txt", safe_part(&step.id)));
            let _ = logs::write_redacted(&f, &t, logs::LOG_MAX_BYTES);
        }
        // A popup whose program exited is gone from the screen.
        if let Some(p) = self.herdr_popup.clone()
            && self.herdr_capture_popup(&h, &p).is_err()
        {
            self.herdr_popup = None;
        }
        self.herdr = Some(h);
        side.duration_ms = t0.elapsed().as_millis() as u64;
        side
    }

    /// Run one step on Herdr with the same timeout as on Vibeke (see [`Run::herdr_step_on`]).
    fn herdr_step(&mut self, step: &Step, ev: &mut Vec<String>) -> (SideResult, Option<String>) {
        let Some(h) = self.herdr.take() else {
            return (SideResult::NotRun, Some(self.herdr_note.clone()));
        };
        let r = self.herdr_step_on(&h, step, ev);
        self.herdr = Some(h);
        r
    }

    /// `server stop`, then the server and the TUI again with the same directories (H5). The
    /// context ids are read again, since Herdr restores its session with new processes.
    fn herdr_restart(&mut self, step: &Step, ev: &mut Vec<String>) -> (SideResult, Option<String>) {
        let Some(h) = self.herdr.take() else {
            return (SideResult::NotRun, Some(self.herdr_note.clone()));
        };
        let logs0: BTreeSet<String> = herdr_logs(&h, &self.lp.plugin_id)
            .iter()
            .filter_map(|l| l["log_id"].as_str().map(str::to_string))
            .collect();
        let mut side = match h.restart() {
            Ok(s) => s,
            Err(e) => {
                self.herdr_note = format!("Herdr did not start again after a restart: {e:#}");
                return (SideResult::HarnessError, Some(self.herdr_note.clone()));
            }
        };
        ev.push("Herdr: server stopped and started again".into());
        self.herdr_panes.clear();
        self.herdr_popup = None;
        if let Err(e) = self.refresh_herdr_context(&side) {
            ev.push(format!("Herdr: {e}"));
        }
        self.subscribe_herdr_events(&mut side);
        self.herdr_off = trace::read(&self.herdr_trace, self.herdr_off)
            .map(|(_, o)| o)
            .unwrap_or(self.herdr_off);
        let from = self.herdr_off;
        let deadline = Instant::now() + self.step_timeout(Some(step));
        let r = self.herdr_expectations(&side, step, &logs0, None, from, deadline, ev);
        self.herdr = Some(side);
        r
    }

    /// After a restart: keep the context and fake agent panes when Herdr restored them,
    /// otherwise take the TUI's focused pane and the pane below it.
    fn refresh_herdr_context(&mut self, side: &HerdrSide) -> Result<(), String> {
        let ids = herdr_pane_ids(side);
        let (workspace, tab, focused) = side
            .active()
            .map_err(|e| format!("no context after the restart: {e:#}"))?;
        if !self.herdr_pane.as_ref().is_some_and(|p| ids.contains(p)) {
            self.herdr_pane = Some(focused);
            self.herdr_workspace = Some(workspace);
            self.herdr_tab = Some(tab);
        }
        if self
            .herdr_agent_pane
            .as_ref()
            .is_some_and(|p| !ids.contains(p))
        {
            return Err("the fake agent pane did not come back after the restart".into());
        }
        Ok(())
    }

    fn herdr_step_on(
        &mut self,
        h: &HerdrSide,
        step: &Step,
        ev: &mut Vec<String>,
    ) -> (SideResult, Option<String>) {
        let id = self.lp.plugin_id.clone();
        let deadline = Instant::now() + self.step_timeout(Some(step));
        let logs0: BTreeSet<String> = herdr_logs(h, &id)
            .iter()
            .filter_map(|l| l["log_id"].as_str().map(str::to_string))
            .collect();
        let from = self.herdr_off;
        let mut opened = None;
        match step.kind {
            StepKind::Action => {
                let action = step.action.clone().unwrap_or_default();
                // The same socket call and context as on Vibeke (H10).
                let context = invocation_context(
                    step.context.as_deref(),
                    self.herdr_workspace.as_deref().unwrap_or_default(),
                    self.herdr_tab.as_deref().unwrap_or_default(),
                    self.herdr_pane.as_deref().unwrap_or_default(),
                    self.herdr_agent_pane.as_deref(),
                );
                match h.call(
                    "plugin.action.invoke",
                    json!({"plugin_id": id, "action_id": self.action_id(&action), "context": context}),
                ) {
                    Ok(r) => ev.push(format!(
                        "Herdr: action {action} launched ({})",
                        r.pointer("/log/log_id").cloned().unwrap_or(Value::Null)
                    )),
                    Err(e) => {
                        return (SideResult::Failed, Some(format!("{e:#}")));
                    }
                }
            }
            StepKind::Pane => {
                let entry = step.pane.clone().unwrap_or_default();
                let placement = self.declared_placement(&entry);
                let args = herdr_pane_open_args(
                    &id,
                    &entry,
                    placement.as_deref(),
                    self.herdr_pane.as_deref(),
                );
                let a: Vec<&str> = args.iter().map(String::as_str).collect();
                match h.cli(&a) {
                    Ok((0, out, _)) => {
                        opened = herdr_opened_pane(&out);
                        if let Some((entry, pane)) = &opened {
                            ev.push(format!("Herdr: pane {entry} opened as {pane}"));
                            self.herdr_panes.insert(entry.clone(), pane.clone());
                        }
                    }
                    Ok((code, _, err)) => {
                        return (
                            SideResult::Failed,
                            Some(format!("herdr {args:?} exited {code}: {err}")),
                        );
                    }
                    Err(e) => return (SideResult::HarnessError, Some(format!("{e:#}"))),
                }
            }
            StepKind::Event if !step.cli.is_empty() => {
                let Ok(st) = &self.herdr_stubs else {
                    return (
                        SideResult::NotRun,
                        Some("the Herdr side has no placeholder values".into()),
                    );
                };
                let args = match step
                    .cli
                    .iter()
                    .map(|a| scenario::expand(a, &st.vars))
                    .collect::<anyhow::Result<Vec<String>>>()
                {
                    Ok(a) => a,
                    Err(e) => {
                        return (
                            SideResult::HarnessError,
                            Some(format!("placeholder on the Herdr side: {e:#}")),
                        );
                    }
                };
                let a: Vec<&str> = args.iter().map(String::as_str).collect();
                match h.cli(&a) {
                    Ok((0, _, _)) => ev.push(format!("Herdr: herdr {}", args.join(" "))),
                    Ok((code, _, err)) => {
                        return (
                            SideResult::Failed,
                            Some(format!("herdr {args:?} exited {code}: {err}")),
                        );
                    }
                    Err(e) => return (SideResult::HarnessError, Some(format!("{e:#}"))),
                }
            }
            // The fake agent reports through the socket, as `herdr pane report-agent --source`
            // would (Herdr requires a source).
            StepKind::Event => {
                let st = step.agent_status.clone().unwrap_or_default();
                let Some(p) = self.herdr_agent_pane.clone() else {
                    return (
                        SideResult::NotRun,
                        Some("the Herdr side has no fake agent pane".into()),
                    );
                };
                if !matches!(st.as_str(), "idle" | "working" | "blocked" | "unknown") {
                    return (
                        SideResult::NotRun,
                        Some(format!("Herdr 0.9.3 has no agent state `{st}`")),
                    );
                }
                if let Err(e) = h.call(
                    "pane.report_agent",
                    json!({"pane_id": p, "source": "vibeke-harness", "agent": "claude", "state": st}),
                ) {
                    return (
                        SideResult::Failed,
                        Some(format!("pane.report_agent: {e:#}")),
                    );
                }
                ev.push(format!("Herdr: fake agent reported {st}"));
            }
            StepKind::Input => {
                if let Err(r) = self.herdr_input(h, step, ev) {
                    return r;
                }
            }
            StepKind::Link => {
                if let Err(r) = self.herdr_link(h, step, ev) {
                    return r;
                }
            }
            StepKind::Lifecycle => {
                let op = match step.lifecycle {
                    Some(LifecycleOp::Enable) => "enable",
                    Some(LifecycleOp::Disable) => "disable",
                    Some(LifecycleOp::Unlink) => "unlink",
                    Some(LifecycleOp::Uninstall) => "uninstall",
                    other => {
                        return (
                            SideResult::NotRun,
                            Some(format!(
                                "the harness runs lifecycle step {other:?} on Vibeke only"
                            )),
                        );
                    }
                };
                match h.cli(&["plugin", op, id.as_str()]) {
                    Ok((0, _, _)) => ev.push(format!("Herdr: herdr plugin {op} {id}")),
                    Ok((code, _, err)) => {
                        return (
                            SideResult::Failed,
                            Some(format!("herdr plugin {op} exited {code}: {err}")),
                        );
                    }
                    Err(e) => return (SideResult::HarnessError, Some(format!("{e:#}"))),
                }
            }
            StepKind::Wait => {}
            StepKind::Restart => unreachable!("restart steps go through herdr_restart"),
        }
        self.herdr_expectations(h, step, &logs0, opened.as_ref(), from, deadline, ev)
    }

    /// Text and keys for a plugin pane on Herdr (H1): `pane.send_input` for a pane with an
    /// id, and the attached TUI client for a popup, which has none.
    fn herdr_input(
        &mut self,
        h: &HerdrSide,
        step: &Step,
        ev: &mut Vec<String>,
    ) -> Result<(), (SideResult, Option<String>)> {
        let pane = step.pane.clone().unwrap_or_default();
        let failed = |w: String| (SideResult::Failed, Some(w));
        let text = match &step.text {
            Some(t) => {
                let Ok(st) = &self.herdr_stubs else {
                    return Err((
                        SideResult::NotRun,
                        Some("the Herdr side has no placeholder values".into()),
                    ));
                };
                Some(scenario::expand(t, &st.vars).map_err(|e| {
                    (
                        SideResult::HarnessError,
                        Some(format!("placeholder on the Herdr side: {e:#}")),
                    )
                })?)
            }
            None => None,
        };
        let keys: Vec<String> = step.keys.iter().map(|k| vibeke_key(k)).collect();
        if self.declared_placement(&pane).as_deref() == Some("popup") {
            if self.herdr_popup.as_deref() != Some(pane.as_str())
                || self.herdr_capture_popup(h, &pane).is_err()
            {
                return Err(failed(format!("popup {pane} is not open on Herdr")));
            }
            if let Some(t) = &text {
                h.write_tui(t.as_bytes())
                    .map_err(|e| (SideResult::HarnessError, Some(format!("{e:#}"))))?;
                ev.push(format!("Herdr: typed text into popup {pane}"));
            }
            for k in &keys {
                let Some(b) = herdr::key_bytes(k) else {
                    return Err((
                        SideResult::HarnessError,
                        Some(format!("no terminal bytes for key {k}")),
                    ));
                };
                // Separate keys arrive as separate reads, as when a person types.
                std::thread::sleep(Duration::from_millis(50));
                h.write_tui(&b)
                    .map_err(|e| (SideResult::HarnessError, Some(format!("{e:#}"))))?;
            }
            if !keys.is_empty() {
                ev.push(format!(
                    "Herdr: typed keys {:?} into popup {pane}",
                    step.keys
                ));
            }
            return Ok(());
        }
        let Some(target) = self.herdr_find_pane(h, &pane, None) else {
            return Err(failed(format!("plugin pane {pane} is not open on Herdr")));
        };
        h.call(
            "pane.send_input",
            json!({"pane_id": target, "text": text.clone().unwrap_or_default(), "keys": keys}),
        )
        .map_err(|e| failed(format!("{e:#}")))?;
        ev.push(format!("Herdr: sent input to {pane} ({target})"));
        Ok(())
    }

    /// A link step on Herdr (H5): print the URL in the context pane, find it on the pane's
    /// screen, and click it with `pane.link.activate`, which runs the matching link handler.
    fn herdr_link(
        &mut self,
        h: &HerdrSide,
        step: &Step,
        ev: &mut Vec<String>,
    ) -> Result<(), (SideResult, Option<String>)> {
        let harness_err = |w: String| (SideResult::HarnessError, Some(w));
        let Ok(st) = &self.herdr_stubs else {
            return Err((
                SideResult::NotRun,
                Some("the Herdr side has no placeholder values".into()),
            ));
        };
        let url = scenario::expand(step.url.as_deref().unwrap_or_default(), &st.vars)
            .map_err(|e| harness_err(format!("{e:#}")))?;
        let Some(pane) = self.herdr_pane.clone() else {
            return Err(harness_err("the Herdr side has no context pane".into()));
        };
        let Some((scheme, rest)) = url.split_once("://") else {
            return Err(harness_err(format!("link URL without a scheme: {url}")));
        };
        // The command line must not show the whole URL, so only the output line matches.
        let cmd = format!(
            "clear; printf '%s%s\\n' {} {}",
            sh_quote(&format!("{scheme}:")),
            sh_quote(&format!("//{rest}"))
        );
        h.call(
            "pane.send_input",
            json!({"pane_id": pane, "text": cmd, "keys": ["enter"]}),
        )
        .map_err(|e| harness_err(format!("print the link: {e:#}")))?;
        let mut at = None;
        poll(Duration::from_secs(10), || {
            at = h
                .call("pane.read", json!({"pane_id": pane, "source": "visible"}))
                .ok()
                .and_then(|r| {
                    let text = r
                        .pointer("/read/text")
                        .or(r.pointer("/text"))
                        .and_then(Value::as_str)?
                        .to_string();
                    link_position(&text, &url)
                });
            at.is_some()
        });
        let Some((row, col)) = at else {
            return Err(harness_err(format!(
                "the link {url} did not show up in Herdr pane {pane}"
            )));
        };
        let r = h
            .call(
                "pane.link.activate",
                json!({"pane_id": pane, "viewport_row": row, "col": col}),
            )
            .map_err(|e| (SideResult::Failed, Some(format!("{e:#}"))))?;
        if r["handled"] != true {
            return Err((
                SideResult::Failed,
                Some(format!("no link handler took {url} on Herdr: {r}")),
            ));
        }
        ev.push(format!(
            "Herdr: link {url} activated at row {row}, col {col}"
        ));
        Ok(())
    }

    /// Wait for a step's expectations and expected calls on Herdr, with the same timeout as on
    /// Vibeke, counted from the Herdr call.
    ///
    /// Herdr passes only when every expectation it can check holds and none is unknown. An
    /// expectation Herdr cannot show makes the result `NotRun` with a reason, so a Vibeke
    /// failure stays suspected (18 §8.4 rule 2).
    #[allow(clippy::too_many_arguments)]
    fn herdr_expectations(
        &mut self,
        h: &HerdrSide,
        step: &Step,
        logs0: &BTreeSet<String>,
        opened: Option<&(String, String)>,
        from: u64,
        deadline: Instant,
        ev: &mut Vec<String>,
    ) -> (SideResult, Option<String>) {
        let id = self.lp.plugin_id.clone();
        let before = &self.herdr_before.clone();
        let mut pending: Vec<&Expectation> = step.expect.iter().collect();
        let mut unknown: Vec<String> = Vec::new();
        let mut calls_ok = step.expect_calls.is_empty();
        let mut mismatches = Vec::new();
        let mut off = from;
        loop {
            let mut still = Vec::new();
            for e in pending {
                match self.look_herdr(h, e, logs0, opened, step.action.as_deref(), before) {
                    Ok(HerdrLook::Holds(x)) => ev.push(format!("Herdr: {x}")),
                    Ok(HerdrLook::NotYet) => still.push(e),
                    Ok(HerdrLook::Unknown(w)) => unknown.push(w),
                    Err(StepErr::Fail(w)) => {
                        self.herdr_off = self.herdr_off.max(off);
                        return (SideResult::Failed, Some(w));
                    }
                    Err(StepErr::Harness(w)) => return (SideResult::HarnessError, Some(w)),
                }
            }
            pending = still;
            if !calls_ok && let Ok((lines, o)) = trace::read(&self.herdr_trace, from) {
                off = o;
                mismatches = trace::match_calls(&lines, &id, &step.expect_calls).0;
                calls_ok = mismatches.is_empty();
            }
            if (pending.is_empty() && calls_ok) || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        self.herdr_off = self.herdr_off.max(off);
        let mut why = Vec::new();
        for e in pending {
            match e {
                // Herdr records a startup command's output only when the command exits, so
                // a long-running command's readiness line cannot be seen.
                Expectation::Ready { text } => unknown.push(format!(
                    "ready {text:?}: Herdr shows startup output only after the command exits"
                )),
                Expectation::Event { event_type } if self.herdr_events_err.is_some() => unknown
                    .push(format!(
                        "event {event_type}: {}",
                        self.herdr_events_err.clone().unwrap_or_default()
                    )),
                Expectation::Popup { pane } => {
                    let title = self.pane_decl(pane).map(|p| p.title.clone());
                    why.push(format!(
                        "no popup frame titled {title:?} for {pane} on Herdr's screen"
                    ))
                }
                Expectation::Pane { pane, placement } => {
                    why.push(self.describe_herdr_pane(h, pane, placement.as_deref(), before))
                }
                other => why.push(format!("no evidence on Herdr for {other:?}")),
            }
        }
        if !calls_ok {
            why.push(format!("expected calls missing on Herdr: {mismatches:?}"));
        }
        // What the step's invocations did (hooks print nothing on success, so their output
        // is kept for event steps too), and the plugin's config dir, which event hooks read.
        let detail = !why.is_empty() || step.kind == StepKind::Event;
        if detail {
            for l in new_invocations(herdr_logs(h, &id), logs0, "event")
                .iter()
                .take(5)
            {
                ev.push(format!("Herdr: {}", invocation_details(l)));
            }
        }
        if step.kind == StepKind::Event
            && let Ok(st) = &self.herdr_stubs
            && let Some(dir) = st.vars.values.get("config")
        {
            let names: Vec<String> = std::fs::read_dir(dir)
                .map(|r| {
                    r.flatten()
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default();
            ev.push(format!(
                "Herdr: plugin config dir holds [{}]",
                names.join(", ")
            ));
        }
        if !why.is_empty() {
            (SideResult::Failed, Some(why.join("; ")))
        } else if !unknown.is_empty() {
            (
                SideResult::NotRun,
                Some(format!(
                    "Herdr cannot check {}; its other expectations held",
                    unknown.join("; ")
                )),
            )
        } else {
            (SideResult::Passed, None)
        }
    }

    /// Why a `pane` expectation did not hold on Herdr.
    fn describe_herdr_pane(
        &self,
        h: &HerdrSide,
        pane: &str,
        placement: Option<&str>,
        before: &BTreeSet<String>,
    ) -> String {
        let Some(title) = self.pane_decl(pane).map(|p| p.title.clone()) else {
            return format!("pane {pane}: the manifest declares no such pane");
        };
        match self.herdr_find_pane(h, pane, Some(before)) {
            None => format!("no new Herdr pane labelled {title:?} for {pane}"),
            Some(id) => format!(
                "Herdr pane {id} ({pane}) is {}, expected {}",
                self.herdr_placement(h, &id)
                    .unwrap_or_else(|| "unknown".into()),
                placement.unwrap_or("any placement")
            ),
        }
    }

    /// One look at an expectation on Herdr. `logs0` are the Herdr log ids from before the
    /// step, `opened` is the plugin pane the step's own Herdr call opened, and `before` the
    /// Herdr pane ids from before the step.
    ///
    /// Plugin panes are ordinary panes in Herdr's `pane.list`, labelled with the manifest's
    /// pane title (`src/app/api/plugins/panes.rs:284`), so the harness finds them by label.
    /// Popups have no pane id: they are found on the screen, at the place `popup_size.rs`
    /// gives them and with the title in their frame. Events come from an `events.subscribe`
    /// connection opened when Herdr started.
    #[allow(clippy::too_many_arguments)]
    fn look_herdr(
        &self,
        h: &HerdrSide,
        e: &Expectation,
        logs0: &BTreeSet<String>,
        opened: Option<&(String, String)>,
        action: Option<&str>,
        before: &BTreeSet<String>,
    ) -> StepRes<HerdrLook> {
        use HerdrLook::{Holds, NotYet, Unknown};
        let id = self.lp.plugin_id.as_str();
        let output = |l: &Value| {
            format!(
                "{}{}",
                l["stdout"].as_str().unwrap_or_default(),
                l["stderr"].as_str().unwrap_or_default()
            )
        };
        Ok(match e {
            Expectation::InvocationFinal { status } => {
                let want = match status.as_deref().unwrap_or("completed") {
                    "completed" | "succeeded" => "succeeded",
                    "failed" => "failed",
                    other => return harness(format!("unknown invocation status `{other}`")),
                };
                let new: Vec<Value> = herdr_logs(h, id)
                    .into_iter()
                    .filter(|l| {
                        l["event"] != "startup"
                            && !logs0.contains(l["log_id"].as_str().unwrap_or_default())
                    })
                    .collect();
                match final_invocation(&new, want, action) {
                    Ok(Some(l)) => Holds(format!("invocation {} {want}", l["log_id"])),
                    Ok(None) => NotYet,
                    Err(why) => return fail(why),
                }
            }
            Expectation::HookLog { text } => herdr_logs(h, id)
                .into_iter()
                .find(|l| {
                    l["event"].is_string()
                        && l["event"] != "startup"
                        && output(l).contains(text.as_str())
                })
                .map_or(NotYet, |l| {
                    Holds(format!("event hook {} logged the text", l["event"]))
                }),
            Expectation::Ready { text } => herdr_logs(h, id)
                .into_iter()
                .find(|l| l["event"] == "startup" && output(l).contains(text.as_str()))
                .map_or(NotYet, |_| Holds("startup command ready".into())),
            Expectation::File { path, contains } => {
                let Ok(st) = &self.herdr_stubs else {
                    return Ok(Unknown("the Herdr side has no placeholder values".into()));
                };
                match scenario::expand(path, &st.vars) {
                    Err(e) => Unknown(format!("file {path}: {e:#}")),
                    Ok(p) => {
                        let p = self.herdr_root.join("home").join(p);
                        let ok =
                            p.exists() && contains.as_deref().is_none_or(|c| read(&p).contains(c));
                        if ok {
                            Holds(format!("file {}", p.display()))
                        } else {
                            NotYet
                        }
                    }
                }
            }
            Expectation::StubRequest { stub, contains } => {
                let Some(log) = self
                    .herdr_stubs
                    .as_ref()
                    .ok()
                    .and_then(|st| st.sinks.get(stub))
                else {
                    return harness(format!("no http_sink stub `{stub}` on the Herdr side"));
                };
                let hit = read(log)
                    .lines()
                    .any(|l| contains.as_deref().is_none_or(|c| l.contains(c)));
                if hit {
                    Holds(format!("stub {stub} received a request"))
                } else {
                    NotYet
                }
            }
            Expectation::Pane { pane, placement } => {
                let found = match opened {
                    Some((entry, pid)) if entry == pane => Some(pid.clone()),
                    _ => self.herdr_find_pane(h, pane, Some(before)),
                };
                match found {
                    None => NotYet,
                    Some(pid) => match placement {
                        None => Holds(format!("pane {pane} opened as {pid}")),
                        Some(p) => {
                            let got = self.herdr_placement(h, &pid);
                            if got.as_deref().map(placement_kind) == Some(placement_kind(p)) {
                                Holds(format!("pane {pane} opened as {pid} at {p}"))
                            } else {
                                NotYet
                            }
                        }
                    },
                }
            }
            Expectation::Popup { pane } => match self.herdr_capture_popup(h, pane) {
                Ok((_, _, note)) => Holds(format!(
                    "a popup frame for {pane} on Herdr's screen{}",
                    note.map(|n| format!(" ({n})")).unwrap_or_default()
                )),
                Err(_) => NotYet,
            },
            Expectation::ScreenText { pane, text, regex } => {
                let re = match regex {
                    Some(r) => {
                        Some(regex::Regex::new(r).map_err(|e| StepErr::Harness(e.to_string()))?)
                    }
                    None => None,
                };
                let Some(placement) = self.declared_placement(pane) else {
                    return Ok(Unknown(format!(
                        "screen text in {pane}: the manifest declares no such pane"
                    )));
                };
                let grid = if placement == "popup" {
                    self.herdr_capture_popup(h, pane).ok().map(|(g, _, _)| g)
                } else {
                    self.herdr_find_pane(h, pane, None).and_then(|pid| {
                        let got = self.herdr_placement(h, &pid).unwrap_or(placement.clone());
                        h.capture_pane(&pid, &got).ok()
                    })
                };
                match grid {
                    None => NotYet,
                    Some(g) => {
                        let screen = g.text();
                        let hit = text.as_deref().is_none_or(|t| screen.contains(t))
                            && re.as_ref().is_none_or(|r| r.is_match(&screen));
                        if hit {
                            Holds(format!("screen text in {pane}"))
                        } else {
                            NotYet
                        }
                    }
                }
            }
            Expectation::Event { event_type } => {
                if let Some(e) = &self.herdr_events_err {
                    return Ok(Unknown(format!("event {event_type}: {e}")));
                }
                let lines = read_from(&self.herdr_events, self.herdr_events_off);
                if lines
                    .lines()
                    .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                    .any(|v| herdr::event_matches(&v, event_type))
                {
                    Holds(format!("event {event_type} (events.subscribe)"))
                } else {
                    NotYet
                }
            }
        })
    }

    /// Herdr's `pane.list`.
    fn herdr_pane_list(&self, h: &HerdrSide) -> Vec<Value> {
        h.call("pane.list", json!({}))
            .ok()
            .and_then(|r| r["panes"].as_array().cloned())
            .unwrap_or_default()
    }

    /// The Herdr pane of the plugin pane entrypoint `name`: a pane labelled with the manifest's
    /// pane title (H2). With `before`, only a pane that was not there before the step counts;
    /// otherwise the pane the harness saw last, or the newest one with that label.
    fn herdr_find_pane(
        &self,
        h: &HerdrSide,
        name: &str,
        before: Option<&BTreeSet<String>>,
    ) -> Option<String> {
        let title = self.pane_decl(name)?.title.clone();
        let ids: Vec<String> = self
            .herdr_pane_list(h)
            .iter()
            .filter(|p| p["label"].as_str() == Some(title.as_str()))
            .filter_map(|p| p["pane_id"].as_str().map(str::to_string))
            .collect();
        match before {
            Some(b) => ids.into_iter().rfind(|p| !b.contains(p)),
            None => self
                .herdr_panes
                .get(name)
                .filter(|p| ids.contains(p))
                .cloned()
                .or_else(|| ids.last().cloned()),
        }
    }

    /// Where Herdr shows the pane: `zoomed` (a zoomed split, which is also how Herdr shows an
    /// overlay), `tab` (alone in a tab other than the context tab) or `split`.
    fn herdr_placement(&self, h: &HerdrSide, pane: &str) -> Option<String> {
        let l = h.layout_of(Some(pane)).ok()?;
        Some(if l.zoomed && l.focused == pane {
            "zoomed".into()
        } else if l.panes.len() == 1 && self.herdr_tab.as_deref() != Some(l.tab.as_str()) {
            "tab".into()
        } else {
            "split".into()
        })
    }

    /// The popup of the pane entrypoint `name` on Herdr's screen (its declared size and title).
    fn herdr_capture_popup(
        &self,
        h: &HerdrSide,
        name: &str,
    ) -> anyhow::Result<(Grid, Rect, Option<String>)> {
        let decl = self
            .pane_decl(name)
            .ok_or_else(|| anyhow::anyhow!("the manifest declares no pane {name}"))?;
        h.capture_popup(
            PopupSize::from_manifest(decl.width.as_ref()),
            PopupSize::from_manifest(decl.height.as_ref()),
            &decl.title,
        )
    }

    /// Follow the popup Herdr shows from its trace: a successful `plugin.pane.open` of a popup
    /// entrypoint of this plugin opens it, `popup.close` closes it.
    fn update_herdr_popup(&mut self, lines: &[trace::TraceLine]) {
        for l in lines {
            match l.method.as_str() {
                "plugin.pane.open" if l.ok => {
                    if let Some(e) = popup_open_entrypoint(l, &self.lp.plugin_id)
                        && self.declared_placement(&e).as_deref() == Some("popup")
                    {
                        self.herdr_popup = Some(e);
                    }
                }
                "popup.close" if l.ok => self.herdr_popup = None,
                _ => {}
            }
        }
    }

    /// The Herdr grid of the plugin pane `name` after a step. A popup is captured only when
    /// this step opened it (or, for a step that opened none of it, while it is still the open
    /// popup), and only with its own title at its declared place (H7). Popups, overlays,
    /// zoomed and tab panes keep their size (H6); a split is resized to the declared size.
    fn capture_herdr(
        &mut self,
        h: &HerdrSide,
        step: &Step,
        name: &str,
        compare: Option<&Compare>,
        lines: &[trace::TraceLine],
    ) -> Option<Capture> {
        if self.herdr_grids >= MAX_GRIDS {
            return None;
        }
        let placement = self.declared_placement(name)?;
        let masks: &[mask::Mask] = compare.map(|c| c.masks.as_slice()).unwrap_or(&[]);
        let mut cap = Capture::default();
        let grid = if placement == "popup" {
            let opened = lines
                .iter()
                .filter(|l| l.method == "plugin.pane.open")
                .filter(|l| popup_open_entrypoint(l, &self.lp.plugin_id).as_deref() == Some(name))
                .map(|l| l.ok)
                .collect::<Vec<bool>>();
            let current = if opened.is_empty() {
                self.herdr_popup.as_deref() == Some(name)
            } else {
                opened.iter().any(|ok| *ok)
            };
            if !current {
                cap.notes.push(format!(
                    "Herdr does not show the {name} popup after this step"
                ));
                return Some(cap);
            }
            match self.herdr_capture_popup(h, name) {
                Ok((g, rect, note)) => {
                    cap.rect = Some(rect);
                    cap.notes.extend(note);
                    cap.placement = Some("popup".into());
                    g
                }
                Err(e) => {
                    cap.notes.push(format!("Herdr capture of {name}: {e:#}"));
                    return Some(cap);
                }
            }
        } else {
            let Some(pid) = self.herdr_find_pane(h, name, None) else {
                cap.notes
                    .push(format!("no Herdr pane labelled for {name} after this step"));
                return Some(cap);
            };
            self.herdr_panes.insert(name.to_string(), pid.clone());
            let got = self.herdr_placement(h, &pid).unwrap_or(placement.clone());
            if let Some(c) = compare
                && let Some(size) = c.size
                && !c.placement_only
                && !fixed_size(&got)
                && let Err(e) = h.resize_pane(&pid, size.cols, size.rows)
            {
                cap.notes.push(format!("Herdr resize of {name}: {e:#}"));
            }
            match h.pane_rect(&pid) {
                Ok(r) => cap.rect = Some(r),
                Err(e) => cap.notes.push(format!("Herdr layout of {name}: {e:#}")),
            }
            cap.placement = Some(got.clone());
            match h.capture_pane(&pid, &got) {
                Ok(g) => g,
                Err(e) => {
                    cap.notes.push(format!("Herdr capture of {name}: {e:#}"));
                    return Some(cap);
                }
            }
        };
        let g = match mask::apply(&grid, masks) {
            Ok(g) => render::redact(&g),
            Err(e) => {
                cap.notes.push(format!("{e:#}"));
                return Some(cap);
            }
        };
        let base = format!("{}__{name}", safe_part(&step.id));
        cap.file = self.write_grid(&format!("{base}.herdr.json"), &g);
        self.herdr_grids += 1;
        cap.grid = Some(g);
        Some(cap)
    }

    // ---- comparison (18 §8.4, §10) ----------------------------------------------------------

    /// One step's result from both sides' records: the visual comparison of its compared panes
    /// (placement, then contents at equal sizes), the screenshots, and the outcome rule.
    fn merge_step(&mut self, step: &Step, v: SideStep, h: SideStep) -> StepResult {
        let mut ev = v.evidence;
        ev.extend(h.evidence);
        let mut vibeke = v.result;
        let mut vreason = v.reason;
        let mut herdr = h.result;
        let mut herdr_reason = h.reason;
        let mut placement_ok: BTreeMap<String, bool> = BTreeMap::new();
        let mut diffs: BTreeMap<String, GridDiff> = BTreeMap::new();
        if vibeke == SideResult::Passed {
            let mut why = v.compare_why.clone();
            let mut herdr_missing = Vec::new();
            for c in &step.compare {
                let (Some(vc), hc) = (v.captures.get(&c.pane), h.captures.get(&c.pane)) else {
                    continue;
                };
                let herdr_judged = herdr == SideResult::Passed;
                match hc {
                    Some(hc) if hc.grid.is_some() => {}
                    Some(hc) if herdr_judged => herdr_missing.push(if hc.notes.is_empty() {
                        format!("no Herdr grid of {}", c.pane)
                    } else {
                        hc.notes.join("; ")
                    }),
                    None if herdr_judged => {
                        herdr_missing.push(format!("no Herdr grid of {}", c.pane))
                    }
                    _ => {}
                }
                let Some(hc) = hc.filter(|_| herdr_judged) else {
                    continue;
                };
                // Placement: both sides' rectangles in their own pane areas.
                let kind = |cap: &Capture| {
                    cap.placement
                        .as_deref()
                        .map(placement_kind)
                        .unwrap_or_default()
                };
                match (vc.rect, hc.rect) {
                    (Some(vr), Some(hr)) => {
                        let m = diff::placement_matches(
                            &Placement {
                                kind: kind(hc),
                                rect: hr,
                            },
                            &Placement {
                                kind: kind(vc),
                                rect: vr,
                            },
                        );
                        placement_ok.insert(c.pane.clone(), m);
                        if !m {
                            why.push(format!(
                                "placement of {} differs from Herdr (Herdr {} {}x{} at row {}, col {}; Vibeke {} {}x{} at row {}, col {})",
                                c.pane,
                                kind(hc),
                                hr.cols,
                                hr.rows,
                                hr.row,
                                hr.col,
                                kind(vc),
                                vr.cols,
                                vr.rows,
                                vr.row,
                                vr.col
                            ));
                        }
                    }
                    _ if kind(vc) != kind(hc) && !kind(vc).is_empty() && !kind(hc).is_empty() => {
                        placement_ok.insert(c.pane.clone(), false);
                        why.push(format!(
                            "{} is {} on Herdr and {} on Vibeke",
                            c.pane,
                            kind(hc),
                            kind(vc)
                        ));
                    }
                    _ => ev.extend(
                        vc.notes
                            .iter()
                            .chain(hc.notes.iter())
                            .map(|n| format!("placement of {} not measured: {n}", c.pane)),
                    ),
                }
                if c.placement_only {
                    continue;
                }
                let (Some(vg), Some(hg)) = (&vc.grid, &hc.grid) else {
                    continue;
                };
                if (vg.cols, vg.rows) == (hg.cols, hg.rows) {
                    let d = diff::compare(hg, vg);
                    if !d.is_empty() {
                        why.push(format!(
                            "{}: {} cell(s) differ from Herdr",
                            c.pane,
                            d.cells.len()
                        ));
                    }
                    diffs.insert(c.pane.clone(), d);
                } else if kind(hc) == "popup" {
                    // Both hosts size a popup from the same declared size and the same pane
                    // area (same terminal and sidebar), so a different size is a difference.
                    why.push(format!(
                        "popup {} is {}x{} on Vibeke and {}x{} on Herdr",
                        c.pane, vg.cols, vg.rows, hg.cols, hg.rows
                    ));
                } else {
                    // Frames differ between the hosts; contents compare only at equal sizes.
                    ev.push(format!(
                        "{}: contents not compared, Herdr shows {}x{} and Vibeke {}x{}",
                        c.pane, hg.cols, hg.rows, vg.cols, vg.rows
                    ));
                }
            }
            if !why.is_empty() {
                vibeke = SideResult::Failed;
                vreason = Some(why.join("; "));
            }
            // Herdr cannot count as a pass for a comparison it could not make: without its
            // side of the evidence, a Vibeke difference stays suspected (18 §8.4).
            if herdr == SideResult::Passed && !herdr_missing.is_empty() {
                herdr = SideResult::NotRun;
                herdr_reason = Some(format!(
                    "Herdr comparison unavailable: {}",
                    herdr_missing.join("; ")
                ));
            }
        }
        // Screenshots: compared panes first, then the other panes either side captured.
        let mut names: Vec<String> = step.compare.iter().map(|c| c.pane.clone()).collect();
        for n in v.captures.keys().chain(h.captures.keys()) {
            if !names.contains(n) {
                names.push(n.clone());
            }
        }
        let mut grids = Vec::new();
        for name in names {
            if self.grid_count >= MAX_GRIDS {
                break;
            }
            let vc = v.captures.get(&name);
            let hc = h.captures.get(&name);
            let vg = vc.and_then(|c| c.grid.as_ref());
            let hg = hc.and_then(|c| c.grid.as_ref());
            if vg.is_none() && hg.is_none() {
                continue;
            }
            let d = diffs.remove(&name).or_else(|| match (vg, hg) {
                (Some(vg), Some(hg)) if (vg.cols, vg.rows) == (hg.cols, hg.rows) => {
                    Some(diff::compare(hg, vg))
                }
                _ => None,
            });
            let base = format!("{}__{name}", safe_part(&step.id));
            let svg =
                render::side_by_side(hg, vg, d.as_ref(), &format!("{} {base}", self.lp.plugin_id));
            let _ = std::fs::create_dir_all(&self.grid_dir);
            let image = std::fs::write(self.grid_dir.join(format!("{base}.svg")), svg)
                .ok()
                .map(|_| format!("{}/{base}.svg", self.grid_rel));
            grids.push(GridArtifact {
                pane: name.clone(),
                vibeke: vc.and_then(|c| c.file.clone()),
                herdr: hc.and_then(|c| c.file.clone()),
                image,
                diff_cells: d.map(|d| d.cells.len() as u32),
                placement_matches: placement_ok.get(&name).copied(),
            });
            self.grid_count += 1;
        }
        let both_ran = matches!(herdr, SideResult::Passed | SideResult::Failed);
        let facts = StepFacts {
            vibeke,
            herdr,
            same_dependency_bytes: match (both_ran, self.lp.dependency_mode) {
                (false, _) => None,
                (true, DependencyMode::Verified) => Some(true),
                // Unpinned: the harness cannot see what each side downloaded yet (build
                // scripts fetch on their own, outside any proxy), so the bytes are unknown
                // rather than known to differ. The rule then gives a Vibeke failure only
                // `suspected_compat_failure` (18 §8.4 rule 3).
                (true, DependencyMode::Unpinned) => None,
            },
            dependency_failed: self.build_failed.is_some(),
            schema_violation: false,
            herdr_reason: herdr_reason.clone(),
        };
        let (actual, reason) = outcome::attribute(&facts);
        let known = step.known_on(self.os);
        let rec = outcome::reconcile(actual, known.map(|k| k.outcome));
        // Both sides' reasons are kept (18 §12): the outcome alone does not say why.
        let head = if rec.outcome == Outcome::Passed {
            None
        } else {
            let head = reason.unwrap_or_else(|| format!("{:?}", rec.outcome));
            Some(match known {
                Some(k) => format!("{head} (known: {})", k.reason),
                None => head,
            })
        };
        let reason = combine_reason(head.as_deref(), vreason.as_deref(), herdr_reason.as_deref());
        StepResult {
            id: step.id.clone(),
            kind: kind_name(step.kind),
            outcome: rec.outcome,
            vibeke,
            herdr,
            known: known.map(|k| k.outcome),
            unexpected_pass: rec.unexpected_pass,
            reason,
            evidence: ev,
            grids,
            duration_ms: v.duration_ms + h.duration_ms,
        }
    }

    // ---- logs and the result ----------------------------------------------------------------

    /// Where diagnostics go: `<results>/<stem>-<os>/logs/`. They are uploaded with the result
    /// for people to read; the report and the screenshot renderer never use them.
    fn logs_dir(&self) -> PathBuf {
        self.grid_dir.join("logs")
    }

    /// The Vibeke side's `plugin.log.list` records, one JSON object per line, redacted.
    fn save_vibeke_plugin_logs(&self) {
        let lines: Vec<String> = self.logs().iter().map(Value::to_string).collect();
        let _ = logs::write_redacted(
            &self.logs_dir().join("vibeke-plugin-logs.jsonl"),
            &lines.join("\n"),
            logs::LOG_MAX_BYTES,
        );
    }

    /// Vibeke's trace, server logs and client screen.
    fn save_logs(&self) {
        let dir = self.logs_dir();
        let max = logs::LOG_MAX_BYTES;
        let _ = logs::copy_redacted(&self.trace, &dir.join("vibeke-trace.jsonl"), max);
        if let Ok(rd) = std::fs::read_dir(self.s.path("state/default/logs")) {
            let mut names: Vec<String> = rd
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| safe_file_name(n))
                .collect();
            names.sort();
            for n in names.into_iter().take(8) {
                let _ = logs::copy_redacted(
                    &self.s.path(&format!("state/default/logs/{n}")),
                    &dir.join(format!("vibeke-{n}")),
                    max,
                );
            }
        }
        if let Some(t) = self.client_screen() {
            let _ = logs::write_redacted(&dir.join("vibeke-client-screen.txt"), &t, max);
        }
    }

    /// The Herdr server log, trace and event log, the last Herdr CLI output, its command logs
    /// and its screen.
    fn save_herdr_logs(&self, h: &HerdrSide) {
        let dir = self.logs_dir();
        let max = logs::LOG_MAX_BYTES;
        let _ = logs::copy_redacted(&h.server_log(), &dir.join("herdr-server.log"), max);
        let _ = logs::copy_redacted(&self.herdr_trace, &dir.join("herdr-trace.jsonl"), max);
        let _ = logs::copy_redacted(&self.herdr_events, &dir.join("herdr-events.jsonl"), max);
        for f in ["cli.out", "cli.err"] {
            let _ = logs::copy_redacted(
                &self.herdr_root.join(f),
                &dir.join(format!("herdr-last-{f}.log")),
                max,
            );
        }
        let lines: Vec<String> = herdr_logs(h, &self.lp.plugin_id)
            .iter()
            .map(Value::to_string)
            .collect();
        if !lines.is_empty() {
            let _ = logs::write_redacted(
                &dir.join("herdr-plugin-logs-end.jsonl"),
                &lines.join("\n"),
                max,
            );
        }
        if let Ok(t) = h.screen_text() {
            let _ = logs::write_redacted(&dir.join("herdr-client-screen.txt"), &t, max);
        }
    }

    fn surfaces(&self) -> Vec<String> {
        let mut out = BTreeSet::new();
        if let Some(m) = &self.manifest {
            if !m.actions_on(self.os).is_empty() {
                out.insert("action".to_string());
            }
            for e in &m.events {
                if m.entry_on(e.platforms.as_ref(), self.os) {
                    out.insert(format!("event:{}", e.on));
                }
            }
            for p in &m.panes {
                if m.entry_on(p.platforms.as_ref(), self.os) {
                    out.insert(format!("pane:{}", p.placement));
                }
            }
            if !m.startup_on(self.os).is_empty() {
                out.insert("startup".to_string());
            }
            if m.link_handlers
                .iter()
                .any(|l| m.entry_on(l.platforms.as_ref(), self.os))
            {
                out.insert("link".to_string());
            }
        }
        if let Ok((lines, _)) = self.trace_since(0)
            && lines.iter().any(|l| l.transport == "socket")
        {
            out.insert("socket".to_string());
        }
        out.into_iter().collect()
    }

    fn coverage(&self) -> CoverageSummary {
        let skipped = self
            .sc
            .skips
            .iter()
            .filter(|s| s.applies_on(self.os))
            .map(|s| s.surface.clone())
            .collect();
        let m = self
            .manifest
            .clone()
            .or_else(|| checked_in_manifest(self.lock, self.lp));
        let Some(m) = m else {
            return CoverageSummary {
                ok: false,
                missing: vec!["the manifest at the locked commit is unavailable".into()],
                skipped,
            };
        };
        let r = coverage::check(self.sc, &m, self.os);
        CoverageSummary {
            ok: r.ok(),
            missing: r
                .missing
                .iter()
                .map(|s| s.name())
                .chain(r.unknown_skips.iter().map(|s| format!("unknown skip {s}")))
                .chain(
                    r.skipped_and_covered
                        .iter()
                        .map(|s| format!("skipped and covered {}", s.name())),
                )
                .collect(),
            skipped,
        }
    }

    /// Both sides one after the other, then the comparison (see the module docs).
    fn run(&mut self) -> PluginResult {
        let mut r = empty_result(self.lp, self.os);
        let sc = self.sc;
        let steps: Vec<&Step> = sc.steps.iter().filter(|s| s.runs_on(self.os)).collect();
        let vibeke = self.vibeke_phase(&steps);
        let herdr = self.herdr_phase(&steps);
        for ((step, v), h) in steps.iter().zip(vibeke).zip(herdr) {
            let res = self.merge_step(step, v, h);
            r.steps.push(res);
        }
        r.lifecycle = std::mem::take(&mut self.lifecycle);
        r.surfaces = self.surfaces();
        r.coverage = self.coverage();
        r
    }
}

/// A file name the result unpacker keeps (`A-Za-z0-9_.-`, short, not `.` or `..`).
fn safe_file_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 80
        && n != "."
        && n != ".."
        && n.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// A step id or pane name as part of a file name (`A-Za-z0-9_.-`, other characters become `_`).
fn safe_part(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The plugin panes a step opens, expects open or types into, for screenshots (and, when the
/// step fails, to show what each pane its expectations name looked like): the pane of a
/// `pane` or `input` step and the panes of its `pane`, `popup` and `screen_text`
/// expectations, each once.
pub fn shot_panes(step: &Step) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut add = |p: &str| {
        if !p.is_empty() && !out.iter().any(|x| x == p) {
            out.push(p.to_string());
        }
    };
    if matches!(step.kind, StepKind::Pane | StepKind::Input)
        && let Some(p) = &step.pane
    {
        add(p);
    }
    for e in &step.expect {
        match e {
            Expectation::Pane { pane, .. }
            | Expectation::Popup { pane }
            | Expectation::ScreenText { pane, .. } => add(pane),
            _ => {}
        }
    }
    out
}

/// Among the invocation records a step started, the one that finished with the wanted status
/// (`succeeded` or `failed`). For an action step only the action's own invocations count, and
/// one that finished with the other status is an immediate failure (`Err`, with its exit
/// code, error and output tail), so the step does not wait out its whole timeout.
pub fn final_invocation<'v>(
    logs: &'v [Value],
    want: &str,
    action: Option<&str>,
) -> Result<Option<&'v Value>, String> {
    // Herdr may record the qualified id (`<plugin>.<action>`).
    let mine = |l: &&Value| {
        action.is_none_or(|a| {
            l["action_id"]
                .as_str()
                .is_some_and(|x| x == a || x.ends_with(&format!(".{a}")))
        })
    };
    if let Some(l) = logs.iter().filter(mine).find(|l| l["status"] == want) {
        return Ok(Some(l));
    }
    // Records without an action id: any invocation the step started counts, as before.
    if !logs.iter().any(|l| mine(&l)) {
        return Ok(logs.iter().find(|l| l["status"] == want));
    }
    if action.is_some()
        && let Some(l) = logs
            .iter()
            .filter(mine)
            .find(|l| matches!(l["status"].as_str(), Some("succeeded" | "failed")))
    {
        return Err(format!(
            "expected the invocation to end {want}: {}",
            invocation_details(l)
        ));
    }
    Ok(None)
}

/// The invocation records that are not in `before` and are not startup commands (`key` is
/// the field that says so: Vibeke's `source`, Herdr's `event`).
fn new_invocations(logs: Vec<Value>, before: &BTreeSet<String>, key: &str) -> Vec<Value> {
    logs.into_iter()
        .filter(|l| {
            l[key] != "startup" && !before.contains(l["log_id"].as_str().unwrap_or_default())
        })
        .collect()
}

/// An evidence line for a failed call no expectation names. Launcher lines (argument parsing
/// in the `herdr` shim) have no method, so they show the first arguments and the parse error.
pub fn describe_failed_call(u: &trace::TraceLine) -> String {
    if u.transport == "launcher" {
        let argv: Vec<&str> = u
            .argv
            .iter()
            .flatten()
            .take(4)
            .map(String::as_str)
            .collect();
        format!(
            "unexpected failed herdr command `{}` ({}): {}",
            argv.join(" "),
            u.code.as_deref().unwrap_or("?"),
            u.parse_error
                .as_deref()
                .unwrap_or("no parse error recorded")
        )
    } else {
        format!(
            "unexpected failed call {} ({})",
            u.method,
            u.code.as_deref().unwrap_or("?")
        )
    }
}

/// Repositories, worktrees, files and plugin config from `[setup]`, for one side: under that
/// side's `home`, expanded with and recorded into that side's `vars` (`{repo.<name>}`,
/// `{worktree.<repo>.<name>}`). Scenarios write plugin config where Herdr keeps it
/// (`~/.config/herdr/plugins/config/<id>`); those files are copied to the side's real plugin
/// config dir `config_dir` (Vibeke's `HERDR_PLUGIN_CONFIG_DIR`, or Herdr's
/// `$XDG_CONFIG_HOME/herdr/plugins/config/<id>`). A setup file at `keep_config` (the Herdr
/// side's own config file, which the harness wrote) goes in front of the harness's settings
/// instead of replacing them.
fn materialize_setup(
    sc: &Scenario,
    plugin_id: &str,
    home: &Path,
    vars: &mut Vars,
    config_dir: Option<&Path>,
    keep_config: Option<&Path>,
) -> StepRes<()> {
    let expand = |s: &str, vars: &Vars| {
        scenario::expand(s, vars).map_err(|e| StepErr::Harness(format!("{e:#}")))
    };
    for r in &sc.setup.repos {
        let dir = home.join("src").join(&r.name);
        std::fs::create_dir_all(&dir).map_err(|e| StepErr::Harness(e.to_string()))?;
        vars.values.insert(
            format!("repo.{}", r.name),
            dir.to_string_lossy().into_owned(),
        );
        for (p, text) in &r.files {
            let text = expand(text, vars)?;
            write(&dir.join(p), &text);
        }
        if r.files.is_empty() {
            write(&dir.join("README.md"), &format!("{}\n", r.name));
        }
        let g = |args: &[&str]| git_try(&dir, args).map_err(StepErr::Harness);
        g(&["init", "-q", "-b", "main"])?;
        g(&["add", "."])?;
        g(&["commit", "-qm", "initial"])?;
        for wt in &r.worktrees {
            let wdir = home.join("src").join(format!("{}-{wt}", r.name));
            g(&[
                "worktree",
                "add",
                "-q",
                "-b",
                wt.as_str(),
                wdir.to_str().unwrap(),
            ])?;
            vars.values.insert(
                format!("worktree.{}.{wt}", r.name),
                wdir.to_string_lossy().into_owned(),
            );
        }
    }
    for (p, text) in &sc.setup.files {
        let p = home.join(expand(p, vars)?);
        let text = expand(text, vars)?;
        match keep_config {
            Some(k) if p == k => {
                let kept = std::fs::read_to_string(k).unwrap_or_default();
                write(&p, &format!("{text}\n{kept}"));
            }
            _ => write(&p, &text),
        }
    }
    if let (Some(cfg), Some(dir)) = (&sc.setup.config, config_dir) {
        let text = expand(cfg, vars)?;
        write(&dir.join("config.toml"), &text);
    }
    let herdr_cfg = home.join(format!(".config/herdr/plugins/config/{plugin_id}"));
    if let Some(dir) = config_dir
        && herdr_cfg.is_dir()
        && herdr_cfg != dir
    {
        copy_tree(&herdr_cfg, dir).map_err(|e| StepErr::Harness(e.to_string()))?;
    }
    Ok(())
}

/// What one look at an expectation on Herdr found.
enum HerdrLook {
    Holds(String),
    NotYet,
    /// Herdr cannot show this expectation; the text says why.
    Unknown(String),
}

/// This plugin's Herdr command logs (`plugin.log.list`), oldest first.
fn herdr_logs(h: &HerdrSide, plugin_id: &str) -> Vec<Value> {
    h.call(
        "plugin.log.list",
        json!({"plugin_id": plugin_id, "limit": 200}),
    )
    .ok()
    .and_then(|v| v["logs"].as_array().cloned())
    .unwrap_or_default()
}

/// The plugin pane that `herdr plugin pane open` printed, as (entrypoint, pane id). The CLI
/// prints the JSON response (`result.type = "plugin_pane_opened"`).
pub fn herdr_opened_pane(stdout: &str) -> Option<(String, String)> {
    std::iter::once(stdout.trim())
        .chain(stdout.lines())
        .filter_map(|t| serde_json::from_str::<Value>(t.trim()).ok())
        .find_map(|v| {
            let pp = v.pointer("/result/plugin_pane").or(v.get("plugin_pane"))?;
            Some((
                pp["entrypoint"].as_str()?.to_string(),
                pp["pane"]["pane_id"].as_str()?.to_string(),
            ))
        })
}

/// A scenario key name in Vibeke's key grammar (07 §2.6.1): tmux spellings (`C-c`, `M-x`)
/// become `ctrl+c`, `alt+x`; named keys (`Enter`, `Escape`, `Tab`) are lowercased; single
/// characters (`q`, `P`) stay as they are, so `P` still means shift+p.
pub fn vibeke_key(k: &str) -> String {
    if k.chars().count() == 1 {
        return k.to_string();
    }
    let mut mods = Vec::new();
    let mut rest = k;
    loop {
        if let Some(r) = rest.strip_prefix("C-") {
            mods.push("ctrl");
            rest = r;
        } else if let Some(r) = rest.strip_prefix("M-") {
            mods.push("alt");
            rest = r;
        } else if let Some(r) = rest.strip_prefix("S-") {
            mods.push("shift");
            rest = r;
        } else {
            break;
        }
    }
    let key = if rest.chars().count() == 1 {
        rest.to_string()
    } else {
        match rest.to_ascii_lowercase().as_str() {
            "return" | "cr" => "enter".to_string(),
            "bspace" => "backspace".to_string(),
            "space" => "space".to_string(),
            other => other.to_string(),
        }
    };
    mods.push(&key);
    mods.join("+")
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let dst = to.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_tree(&e.path(), &dst)?;
        } else {
            std::fs::copy(e.path(), dst)?;
        }
    }
    Ok(())
}

fn remaining(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

/// The text of `path` from byte `from` (empty when it is missing or shorter).
fn read_from(path: &Path, from: u64) -> String {
    let b = std::fs::read(path).unwrap_or_default();
    String::from_utf8_lossy(b.get(from as usize..).unwrap_or_default()).into_owned()
}

/// Every Herdr pane id (`pane.list`).
fn herdr_pane_ids(h: &HerdrSide) -> BTreeSet<String> {
    h.call("pane.list", json!({}))
        .ok()
        .and_then(|r| r["panes"].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|p| p["pane_id"].as_str().map(str::to_string))
        .collect()
}

/// The entrypoint of a traced `plugin.pane.open` call of `plugin_id` (the proxy keeps the
/// plugin, entrypoint and placement of that call). Calls without a plugin id count too.
pub fn popup_open_entrypoint(l: &trace::TraceLine, plugin_id: &str) -> Option<String> {
    let p = l.params.as_ref()?;
    if p["plugin_id"].as_str().is_some_and(|x| x != plugin_id) {
        return None;
    }
    p["entrypoint"].as_str().map(str::to_string)
}

/// Every running process (pid to `ps` state), without zombies. Empty when `ps` fails.
fn live_pids() -> BTreeSet<u32> {
    let Ok(out) = Command::new("ps").args(["-A", "-o", "pid=,stat="]).output() else {
        return BTreeSet::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let pid: u32 = it.next()?.parse().ok()?;
            let stat = it.next().unwrap_or("");
            (!stat.starts_with('Z')).then_some(pid)
        })
        .collect()
}

/// Whether `value` (an environment value, possibly a `:`-separated list) names `root` or a
/// path under it.
pub fn names_path_under(value: &str, root: &str) -> bool {
    let root = root.trim_end_matches('/');
    !root.is_empty()
        && value.split(':').any(|part| {
            part == root
                || part
                    .strip_prefix(root)
                    .is_some_and(|rest| rest.starts_with('/'))
        })
}

/// The processes (other than this one) whose working directory or any environment value is
/// inside one of `roots`: everything a side started, also daemons in their own sessions.
fn processes_under(roots: &[String]) -> BTreeSet<u32> {
    let me = std::process::id();
    live_pids()
        .into_iter()
        .filter(|&pid| pid != me && pid > 1)
        .filter(|&pid| {
            let cwd = vk_hold::procinfo::cwd(pid).unwrap_or_default();
            let env = vk_hold::procinfo::environ(pid);
            roots.iter().any(|r| {
                names_path_under(&cwd, r) || env.iter().any(|(_, v)| names_path_under(v, r))
            })
        })
        .collect()
}

/// The TCP ports `pids` listen on (`lsof`; empty when it is missing).
fn listening_ports(pids: &BTreeSet<u32>) -> BTreeSet<u16> {
    if pids.is_empty() {
        return BTreeSet::new();
    }
    let list: Vec<String> = pids.iter().map(u32::to_string).collect();
    let Ok(out) = Command::new("lsof")
        .args(["-nP", "-a", "-iTCP", "-sTCP:LISTEN", "-Fn", "-p"])
        .arg(list.join(","))
        .output()
    else {
        return BTreeSet::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.strip_prefix('n')?.rsplit(':').next()?.parse().ok())
        .collect()
}

/// SIGKILL `pids` and their process groups (never this process's own group).
fn kill_processes(pids: &BTreeSet<u32>) {
    // SAFETY: getpgrp has no preconditions.
    let own = unsafe { libc::getpgrp() };
    for &pid in pids {
        if let Some(i) = vk_hold::procinfo::info(pid)
            && i.pgid > 1
            && i.pgid as libc::pid_t != own
        {
            // SAFETY: kill has no memory-safety preconditions.
            unsafe { libc::kill(-(i.pgid as libc::pid_t), libc::SIGKILL) };
        }
        // SAFETY: as above.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    }
}

/// Whether nothing listens on `port` on the loopback address.
fn port_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}
