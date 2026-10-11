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
//! checks (18 §8.3). The Herdr side runs only when `VIBEKE_HERDR_BIN` is set. Every result goes
//! into one `PluginResult` file; only harness errors, unexpected passes and coverage failures
//! fail the test (other outcomes are data for the report).

use super::Session;
use super::plugins::{poll, read, write};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use vk_compat::herdr::manifest::Manifest;
use vk_compat::herdr::registry;
use vk_conformance::diff::{self, GridDiff, Placement};
use vk_conformance::grid::{Grid, Rect};
use vk_conformance::herdr::{self, HerdrOptions, HerdrSide};
use vk_conformance::lock::{DependencyMode, Lock, LockedPlugin};
use vk_conformance::outcome::{self, Outcome, SideResult, StepFacts};
use vk_conformance::result::{
    CheckResult, CoverageSummary, GridArtifact, PluginResult, SCHEMA, StepResult,
};
use vk_conformance::scenario::{
    self, Compare, Expectation, LifecycleOp, Scenario, Step, StepKind, StubKind, Vars,
};
use vk_conformance::{coverage, mask, render, trace};

/// The repository root.
pub fn workspace_root() -> PathBuf {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    p.canonicalize().unwrap_or(p)
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

/// Where result files go: `$VIBEKE_HERDR_PLUGIN_RESULTS`, or `target/herdr-plugins`.
pub fn results_dir() -> PathBuf {
    std::env::var_os("VIBEKE_HERDR_PLUGIN_RESULTS")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root().join("target/herdr-plugins"))
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
/// toolchain variables the workflow sets, but never secret-looking ones.
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
];

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

fn check(name: &str, outcome: Outcome, reason: impl Into<Option<String>>) -> CheckResult {
    CheckResult {
        check: name.to_string(),
        outcome,
        reason: reason.into(),
    }
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
    let mut result = if !lp.supports(os) {
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
    stub_env: Vec<(String, String)>,
    sinks: BTreeMap<String, PathBuf>,
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
    /// The grid files of this run: `<results>/<stem>-<os>/`.
    grid_dir: PathBuf,
    grid_rel: String,
    lifecycle: Vec<CheckResult>,
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
        let s = Session::builder()
            .prefix("vkhp")
            .home(&home)
            .allow_only(&env_allowlist())
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
        for (k, v) in [("home", &home), ("state", &state), ("config", &config)] {
            vars.values
                .insert(k.into(), v.to_string_lossy().into_owned());
        }
        // Stubs: their URLs and files are placeholders in the session's environment.
        let mut sinks = BTreeMap::new();
        for st in &sc.stubs {
            let file = match st.kind {
                StubKind::HttpSink => {
                    let log = rp.join(format!("stubs/{}.jsonl", st.id));
                    std::fs::create_dir_all(log.parent().unwrap()).map_err(|e| e.to_string())?;
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
                        .unwrap_or_else(|| rp.join(format!("stubs/{}", st.id)));
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
        let mut stub_env = Vec::new();
        for st in &sc.stubs {
            for (k, v) in &st.env {
                stub_env.push((
                    k.clone(),
                    scenario::expand(v, &vars).map_err(|e| format!("stub {}: {e:#}", st.id))?,
                ));
            }
        }
        let mut s = s;
        s.env.extend(stub_env.iter().cloned());
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
            root,
            s,
            trace,
            trace_off: 0,
            seq: 0,
            vars,
            ctx: Context::default(),
            stub_env,
            sinks,
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

    fn herdr_cli(&self, args: &[String]) -> Result<Value, String> {
        let mut a = vec!["compat", "herdr"];
        a.extend(args.iter().map(String::as_str));
        self.cli(&a)
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

    fn restart(&mut self) {
        if self.s.server_pid() > 0 {
            self.s.kill_server();
        }
        let _ = self.cli(&["pane", "list"]);
        self.activations += 1;
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
        let home = self.home().to_string_lossy().into_owned();
        let w = self.herdr_cli(&["workspace".into(), "create".into(), "--cwd".into(), home])?;
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
        self.new_events();
        Ok(())
    }

    /// Repositories, worktrees, files and plugin config from `[setup]`. Runs after install and
    /// before trust, so `{state}` is known and no plugin code has run yet.
    fn setup_files(&mut self) -> StepRes<()> {
        let home = self.home();
        for r in &self.sc.setup.repos {
            let dir = home.join("src").join(&r.name);
            std::fs::create_dir_all(&dir).map_err(|e| StepErr::Harness(e.to_string()))?;
            self.vars.values.insert(
                format!("repo.{}", r.name),
                dir.to_string_lossy().into_owned(),
            );
            for (p, text) in &r.files {
                let text = self.expand(text)?;
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
                self.vars.values.insert(
                    format!("worktree.{}.{wt}", r.name),
                    wdir.to_string_lossy().into_owned(),
                );
            }
        }
        for (p, text) in &self.sc.setup.files {
            let p = self.expand(p)?;
            let text = self.expand(text)?;
            write(&home.join(p), &text);
        }
        if let (Some(cfg), Some(dir)) = (&self.sc.setup.config, &self.config_dir) {
            let text = self.expand(cfg)?;
            write(&dir.join("config.toml"), &text);
        }
        // Scenarios write plugin config where Herdr keeps it. Vibeke gives a legacy plugin
        // its own config dir (`HERDR_PLUGIN_CONFIG_DIR`, next to Vibeke's config), so the
        // files are copied there too.
        let herdr_cfg = home.join(format!(
            ".config/herdr/plugins/config/{}",
            self.lp.plugin_id
        ));
        if let Some(dir) = &self.config_dir
            && herdr_cfg.is_dir()
        {
            copy_tree(&herdr_cfg, dir).map_err(|e| StepErr::Harness(e.to_string()))?;
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
        match self.trust("") {
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

    // ---- Herdr side (18 §11) ----------------------------------------------------------------

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
        let hroot = self.root.path().canonicalize().unwrap().join("herdr");
        let _ = std::fs::create_dir_all(&hroot);
        let opts = HerdrOptions {
            bin,
            root: hroot,
            cols: 120,
            rows: 40,
            trace: self.herdr_trace.clone(),
            env: self.stub_env.clone(),
        };
        let side = match HerdrSide::start(opts) {
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
        let dir = match self.clone_dir() {
            Ok(d) => d,
            Err(e) => {
                self.herdr_note = format!("Herdr side not set up: {e}");
                return;
            }
        };
        match side.cli(&["plugin", "install", dir.to_str().unwrap()]) {
            Ok((0, _, _)) => {
                self.herdr = Some(side);
                self.herdr_note.clear();
            }
            Ok((code, _, err)) => {
                self.herdr_note = format!("Herdr could not install the plugin ({code}): {err}");
                let _ = side.stop();
            }
            Err(e) => {
                self.herdr_note = format!("Herdr could not install the plugin: {e:#}");
                let _ = side.stop();
            }
        }
    }

    /// Run one step on Herdr: the same Herdr CLI the step maps to, then its expected calls.
    fn herdr_step(&mut self, step: &Step, deadline: Instant) -> (SideResult, Option<String>) {
        let Some(h) = &self.herdr else {
            return (SideResult::NotRun, Some(self.herdr_note.clone()));
        };
        let id = &self.lp.plugin_id;
        let args: Vec<String> = match step.kind {
            StepKind::Action => vec![
                "plugin".into(),
                "action".into(),
                "invoke".into(),
                format!("{id}.{}", step.action.clone().unwrap_or_default()),
            ],
            StepKind::Pane => vec![
                "plugin".into(),
                "pane".into(),
                "open".into(),
                id.clone(),
                step.pane.clone().unwrap_or_default(),
            ],
            StepKind::Event if !step.cli.is_empty() => {
                match step
                    .cli
                    .iter()
                    .map(|a| self.expand(a))
                    .collect::<StepRes<Vec<String>>>()
                {
                    Ok(a) => a,
                    Err(_) => return (SideResult::HarnessError, Some("placeholder".into())),
                }
            }
            _ => {
                return (
                    SideResult::NotRun,
                    Some(format!(
                        "{} steps run on Vibeke only for now",
                        kind_name(step.kind)
                    )),
                );
            }
        };
        let a: Vec<&str> = args.iter().map(String::as_str).collect();
        match h.cli(&a) {
            Ok((0, _, _)) => {}
            Ok((code, _, err)) => {
                return (
                    SideResult::Failed,
                    Some(format!("herdr {args:?} exited {code}: {err}")),
                );
            }
            Err(e) => return (SideResult::HarnessError, Some(format!("{e:#}"))),
        }
        if step.expect_calls.is_empty() {
            return (SideResult::Passed, None);
        }
        let from = self.herdr_off;
        let mut mismatches = Vec::new();
        let mut off = from;
        poll(remaining(deadline), || {
            if let Ok((lines, o)) = trace::read(&self.herdr_trace, from) {
                off = o;
                mismatches = trace::match_calls(&lines, id, &step.expect_calls).0;
            }
            mismatches.is_empty()
        });
        self.herdr_off = off;
        if mismatches.is_empty() {
            (SideResult::Passed, None)
        } else {
            (
                SideResult::Failed,
                Some(format!("expected calls missing on Herdr: {mismatches:?}")),
            )
        }
    }

    // ---- steps (18 §8) ----------------------------------------------------------------------

    fn context_json(&self, context: Option<&str>) -> Value {
        let c = &self.ctx;
        match context.unwrap_or("global") {
            "workspace" => json!({"workspace_id": c.workspace}),
            "tab" => json!({"workspace_id": c.workspace, "tab_id": c.tab}),
            "pane" | "selection" => {
                json!({"workspace_id": c.workspace, "tab_id": c.tab, "pane_id": c.pane})
            }
            _ => json!({}),
        }
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
                let r = self
                    .api(
                        "plugin.action.run",
                        json!({"plugin": id, "action": action, "context": self.context_json(step.context.as_deref())}),
                    )
                    .or_else(fail)?;
                ev.push(format!("action {action} launched ({})", r["log"]["log_id"]));
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
                for h in &handlers {
                    match self.api(
                        "plugin.link.open",
                        json!({"plugin": id, "handler": h, "url": url, "context": self.context_json(Some("pane"))}),
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
                let r = self
                    .api(
                        "compat.herdr.call",
                        json!({"method": "plugin.pane.open", "params": {"plugin_id": id, "pane": pane, "pane_id": self.ctx.pane}}),
                    )
                    .or_else(fail)?;
                if !r["error"].is_null() {
                    return fail(format!("plugin.pane.open: {}", r["error"]));
                }
                ev.push(format!(
                    "pane {pane} opened ({})",
                    r["result"]["placement"].as_str().unwrap_or("?")
                ));
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
    ) -> StepRes<Option<String>> {
        let id = self.lp.plugin_id.as_str();
        Ok(match e {
            Expectation::InvocationFinal { status } => {
                let want = match status.as_deref().unwrap_or("completed") {
                    "completed" | "succeeded" => "succeeded",
                    "failed" => "failed",
                    other => return harness(format!("unknown invocation status `{other}`")),
                };
                self.logs()
                    .into_iter()
                    .filter(|l| {
                        l["source"] != "startup"
                            && !logs0.contains(l["log_id"].as_str().unwrap_or_default())
                    })
                    .find(|l| l["status"] == want)
                    .map(|l| format!("invocation {} {want}", l["log_id"]))
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
                // Scenarios name Herdr events (`worktree.created`): a Vibeke event of that
                // type, or one the compat projection turns into it, is the evidence.
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
                            || proj
                                .project(&input)
                                .iter()
                                .any(|n| *n == event_type.as_str())
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
                match self.look(e, seq0, logs0)? {
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
            ev.push(format!(
                "unexpected failed call {} ({:?})",
                u.method, u.code
            ));
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

    fn write_grid(&self, name: &str, g: &Grid) -> Option<String> {
        let _ = std::fs::create_dir_all(&self.grid_dir);
        let json = serde_json::to_string_pretty(g).ok()?;
        std::fs::write(self.grid_dir.join(name), json).ok()?;
        Some(format!("{}/{name}", self.grid_rel))
    }

    /// Visual comparison of one plugin pane (18 §10): placement, then contents when both sides
    /// see the same inner size, then the reference snapshot (18 §10.5).
    fn compare(
        &self,
        step: &Step,
        c: &Compare,
        herdr_ran: bool,
        deadline: Instant,
        ev: &mut Vec<String>,
        why: &mut Vec<String>,
    ) -> StepRes<GridArtifact> {
        let mut art = GridArtifact {
            pane: c.pane.clone(),
            vibeke: None,
            herdr: None,
            image: None,
            diff_cells: None,
            placement_matches: None,
        };
        let Some((pane, placement)) = self.plugin_pane_placed(&c.pane) else {
            why.push(format!("plugin pane {} not found for comparison", c.pane));
            return Ok(art);
        };
        let vp = self.s.pane(&pane);
        let v_place = Placement {
            kind: if placement == "split" {
                "split-right".into()
            } else {
                placement.clone()
            },
            rect: Rect {
                row: 0,
                col: 0,
                rows: vp["rows"].as_u64().unwrap_or(0) as u16,
                cols: vp["cols"].as_u64().unwrap_or(0) as u16,
            },
        };
        let herdr = if herdr_ran { self.herdr.as_ref() } else { None };
        if let Some(h) = herdr
            && let Ok(rect) = h.pane_rect(&c.pane)
        {
            let h_place = Placement {
                kind: v_place.kind.clone(),
                rect,
            };
            let m = diff::placement_matches(&h_place, &v_place);
            art.placement_matches = Some(m);
            if !m {
                why.push(format!("placement of {} differs from Herdr", c.pane));
            }
        }
        let mut contents = !c.placement_only;
        if let (true, Some(size)) = (contents, c.size) {
            if !self.resize_to(&pane, size.cols, size.rows) {
                ev.push(format!(
                    "{}: could not resize to {}x{}; placement only",
                    c.pane, size.cols, size.rows
                ));
                contents = false;
            }
            if let Some(h) = herdr
                && h.resize_pane(&c.pane, size.cols, size.rows).is_err()
            {
                contents = false;
            }
        }
        if !contents {
            return Ok(art);
        }
        let stable = Duration::from_millis(c.stable_ms.unwrap_or(500));
        let (vg, is_stable) = mask::wait_stable(
            || self.capture(&pane),
            &c.masks,
            stable,
            remaining(deadline).max(Duration::from_secs(5)),
        )
        .map_err(|e| StepErr::Harness(format!("capture {}: {e:#}", c.pane)))?;
        if !is_stable {
            why.push(format!("{} never became stable", c.pane));
        }
        let vg = render::redact(&vg);
        let base = format!("{}__{}", step.id, c.pane);
        art.vibeke = self.write_grid(&format!("{base}.vibeke.json"), &vg);
        let mut hg = None;
        let mut gd: Option<GridDiff> = None;
        if let Some(h) = herdr {
            match h
                .capture_pane(&c.pane, &placement)
                .and_then(|g| mask::apply(&g, &c.masks))
            {
                Ok(g) => {
                    let g = render::redact(&g);
                    let d = diff::compare(&g, &vg);
                    art.diff_cells = Some(d.cells.len() as u32);
                    if !d.is_empty() {
                        why.push(format!(
                            "{}: {} cell(s) differ from Herdr",
                            c.pane,
                            d.cells.len()
                        ));
                    }
                    art.herdr = self.write_grid(&format!("{base}.herdr.json"), &g);
                    hg = Some(g);
                    gd = Some(d);
                }
                Err(e) => ev.push(format!("Herdr capture of {} failed: {e:#}", c.pane)),
            }
        }
        let svg = render::side_by_side(
            hg.as_ref(),
            Some(&vg),
            gd.as_ref(),
            &format!("{} {}", self.lp.plugin_id, base),
        );
        let _ = std::fs::create_dir_all(&self.grid_dir);
        if std::fs::write(self.grid_dir.join(format!("{base}.svg")), svg).is_ok() {
            art.image = Some(format!("{}/{base}.svg", self.grid_rel));
        }
        // Reviewed reference snapshots of Vibeke's own grid (18 §10.5).
        let snap = plugins_dir().join(format!("snapshots/{}/{base}.json", self.stem));
        if std::env::var("VIBEKE_UPDATE_SNAPSHOTS").as_deref() == Ok("1") {
            write(
                &snap,
                &serde_json::to_string_pretty(&vg).unwrap_or_default(),
            );
            ev.push(format!("snapshot {} written", snap.display()));
        } else if snap.exists() {
            match serde_json::from_str::<Grid>(&read(&snap)) {
                Ok(reference) => {
                    let reference = mask::apply(&reference, &c.masks)
                        .map_err(|e| StepErr::Harness(format!("{e:#}")))?;
                    let d = diff::compare(&reference, &vg);
                    if d.is_empty() {
                        ev.push(format!("{} matches its reference snapshot", c.pane));
                    } else {
                        ev.push(format!(
                            "{}: {} cell(s) differ from the reference snapshot",
                            c.pane,
                            d.cells.len()
                        ));
                        why.push(format!("{} differs from its reference snapshot", c.pane));
                    }
                }
                Err(e) => return harness(format!("{}: {e}", snap.display())),
            }
        }
        Ok(art)
    }

    fn run_step(&mut self, step: &Step) -> StepResult {
        let t0 = Instant::now();
        let deadline = t0 + self.step_timeout(Some(step));
        let mut ev = Vec::new();
        let mut grids = Vec::new();
        self.new_events();
        let seq0 = self.seq;
        let trace0 = match self.trace_since(self.trace_off) {
            Ok((_, off)) => off,
            Err(_) => self.trace_off,
        };
        self.trace_off = trace0;
        self.herdr_off = trace::read(&self.herdr_trace, self.herdr_off)
            .map(|(_, o)| o)
            .unwrap_or(self.herdr_off);
        let logs0: BTreeSet<String> = self
            .logs()
            .iter()
            .filter_map(|l| l["log_id"].as_str().map(str::to_string))
            .collect();

        let (vibeke, vreason) = if let Some(b) = &self.build_failed {
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
        let (herdr, herdr_reason) = if self.build_failed.is_some() {
            (SideResult::NotRun, Some("the Vibeke build failed".into()))
        } else {
            self.herdr_step(step, deadline)
        };
        let mut vibeke = vibeke;
        let mut vreason = vreason;
        if vibeke == SideResult::Passed {
            let mut why = Vec::new();
            for c in &step.compare {
                match self.compare(
                    step,
                    c,
                    herdr == SideResult::Passed,
                    deadline,
                    &mut ev,
                    &mut why,
                ) {
                    Ok(a) => grids.push(a),
                    Err(StepErr::Harness(w)) => {
                        vibeke = SideResult::HarnessError;
                        why.push(w);
                    }
                    Err(StepErr::Fail(w)) => why.push(w),
                }
            }
            if !why.is_empty() {
                if vibeke == SideResult::Passed {
                    vibeke = SideResult::Failed;
                }
                vreason = Some(why.join("; "));
            }
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
                // rather than known to differ.
                (true, DependencyMode::Unpinned) => None,
            },
            dependency_failed: self.build_failed.is_some(),
            schema_violation: false,
            herdr_reason: herdr_reason.clone(),
        };
        let (actual, reason) = outcome::attribute(&facts);
        let known = step.known.as_ref().map(|k| k.outcome);
        let rec = outcome::reconcile(actual, known);
        let reason = if rec.outcome == Outcome::Passed {
            reason.or(vreason)
        } else {
            reason
                .or(vreason)
                .or(herdr_reason)
                .or_else(|| step.known.as_ref().map(|k| k.reason.clone()))
                .or_else(|| Some(format!("{:?}", rec.outcome)))
        };
        StepResult {
            id: step.id.clone(),
            kind: kind_name(step.kind),
            outcome: rec.outcome,
            vibeke,
            herdr,
            known,
            unexpected_pass: rec.unexpected_pass,
            reason,
            evidence: ev,
            grids,
            duration_ms: t0.elapsed().as_millis() as u64,
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
        let skipped = self.sc.skips.iter().map(|s| s.surface.clone()).collect();
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

    fn run(&mut self) -> PluginResult {
        let mut r = empty_result(self.lp, self.os);
        if let Err(e) = self.context() {
            self.lifecycle
                .push(check("session", Outcome::HarnessError, e));
        } else if self.install() {
            match self.setup_files() {
                Err(StepErr::Harness(e) | StepErr::Fail(e)) => {
                    self.lifecycle
                        .push(check("setup", Outcome::HarnessError, e));
                }
                Ok(()) => {
                    self.start_herdr();
                    self.trust_and_build();
                    if self.build_failed.is_none() {
                        self.activate();
                    }
                    let sc = self.sc;
                    for step in &sc.steps {
                        let res = self.run_step(step);
                        r.steps.push(res);
                    }
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
            }
        }
        if let Some(h) = self.herdr.take() {
            let _ = h.stop();
        }
        r.lifecycle = std::mem::take(&mut self.lifecycle);
        r.surfaces = self.surfaces();
        r.coverage = self.coverage();
        r
    }
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
