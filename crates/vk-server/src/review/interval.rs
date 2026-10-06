//! Execution-interval binding of observed agent commands (15 §6.3, lane 3F; pure rules in
//! `vk_review::interval`).
//!
//! An observed command is evidence for a subject only when the checkout stayed that subject for
//! the whole command. For every shell command of a run bound to a task this module:
//!
//! 1. at the **start** (the synchronous `PreToolUse` hook): makes sure a file watcher covers the
//!    checkout (journal of non-ignored writes, armed before the command starts), captures the
//!    checkout state (HEAD, change digest) and the other runs writing in the checkout;
//! 2. at the **end** (`PostToolUse`): waits `settle_ms` for late watcher events, captures the
//!    state again, asks Git which written paths are ignored, and lets
//!    [`vk_review::interval::decide`] bind or refuse (matching start/end digests alone never
//!    bind: a write-test-revert shows up in the journal);
//! 3. persists one `command_interval` record per command. `task.review.get` then names the
//!    subject on a command only for the selected subject that state shows exactly.
//!
//! All Git and watcher work runs on one worker thread, never under the core lock or on the
//! render path. Config (`[review.interval]`): `enabled`, `watcher = "os" | "none"` (`none` is
//! the fake backend: nothing is ever bound), `settle_ms`, `arm_delay_ms`, `harnesses` (only
//! harnesses whose pre-tool hook is known to run *before* the tool; default `["claude"]`).
//! Tests feed the journal directly through [`install_manual`] and [`feed`].

use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use vk_review::interval::{
    self as iv, IntervalBinding, IntervalInput, IntervalStatus, UnboundReason, WriteJournal,
};
use vk_review::screenshot::{CodeState, capture_code_state};

pub const METHODS: &[(&str, bool)] = &[
    ("task.review.intervals", false),
    ("task.review.interval_status", false),
];

/// Store entity kind (one row per observed shell command; closed once decided).
pub const K_INTERVAL: &str = "command_interval";

/// Most paths asked of `git check-ignore` at once; further paths count as not ignored.
const MAX_IGNORE_PATHS: usize = 5000;
/// Watched checkouts kept at once; the least recently used idle one is dropped first.
const MAX_WATCHED: usize = 8;
/// Journal events older than this are dropped when the checkout is used again.
const JOURNAL_KEEP_MS: i64 = 3_600_000;

// ---- config ---------------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct IntervalConfig {
    pub enabled: bool,
    /// `os` (the platform file watcher) or `none` (fake: nothing is ever bound).
    pub watcher: String,
    /// How long after a command ends late watcher events still count for it.
    pub settle_ms: u64,
    /// A watcher that was just started only counts as armed after this delay (the platform
    /// stream needs a moment before it delivers).
    pub arm_delay_ms: u64,
    /// Harnesses whose pre-tool hook is synchronous (runs before the tool starts).
    pub harnesses: Vec<String>,
    pub max_events: usize,
    /// A watcher no command used for this long is dropped.
    pub idle_drop_secs: u64,
}

impl Default for IntervalConfig {
    fn default() -> Self {
        IntervalConfig {
            enabled: true,
            watcher: "os".into(),
            settle_ms: 400,
            arm_delay_ms: 500,
            harnesses: vec!["claude".into()],
            max_events: iv::DEFAULT_MAX_EVENTS,
            idle_drop_secs: 1800,
        }
    }
}

impl IntervalConfig {
    pub fn load() -> Self {
        let cfg = vk_config::Config::load(vk_config::config_path())
            .map(|(c, _)| c)
            .unwrap_or_default();
        Self::from_config(&cfg)
    }

    pub fn from_config(cfg: &vk_config::Config) -> Self {
        cfg.extra
            .get("review")
            .and_then(|t| t.get("interval"))
            .and_then(|t| serde_json::to_value(t).ok())
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default()
    }
}

// ---- records --------------------------------------------------------------------------------

/// One observed shell command and its execution interval.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntervalRec {
    /// `<run>:<tool_use_id>`, the id of the tool item.
    pub id: String,
    pub run: String,
    pub item: String,
    #[serde(default)]
    pub command: Option<String>,
    pub repo_root: String,
    pub start_ms: i64,
    #[serde(default)]
    pub end_ms: Option<i64>,
    #[serde(default)]
    pub start_state: Option<CodeState>,
    #[serde(default)]
    pub start_error: Option<String>,
    /// Other runs writing in the same checkout at the start.
    #[serde(default)]
    pub other_writers: Vec<String>,
    #[serde(default)]
    pub binding: Option<IntervalBinding>,
    /// `os`, `manual` (tests) or `none`.
    pub watcher: String,
    /// Identity of every tool the command runs, probed when it ended (repo-relative programs
    /// belong to the subject and have none). Lets a bound command carry the environment
    /// identity a mapped check would record.
    #[serde(default)]
    pub tools: BTreeMap<String, String>,
    /// Tools whose identity was looked up (including repo-relative ones, which have none).
    #[serde(default)]
    pub tools_probed: BTreeSet<String>,
    /// A tool's identity was unavailable (probe failed): the environment stays unknown.
    #[serde(default)]
    pub tools_error: Option<String>,
}

/// Probe the identity of every tool of `command` (host environment, like a host check run).
fn probe_tools(
    command: &str,
    repo: &Path,
) -> (BTreeMap<String, String>, BTreeSet<String>, Option<String>) {
    let (mut tools, mut probed) = (BTreeMap::new(), BTreeSet::new());
    for t in command_tools(&CheckCommand::Shell(command.to_string())) {
        match tool_identity(&t, repo) {
            Ok(Some(v)) => {
                tools.insert(t.clone(), v);
            }
            Ok(None) => {}
            Err(e) => return (tools, probed, Some(e)),
        }
        probed.insert(t);
    }
    (tools, probed, None)
}

// ---- watcher registry -----------------------------------------------------------------------

struct Watched {
    journal: Arc<Mutex<WriteJournal>>,
    kind: &'static str,
    _watcher: Option<notify::RecommendedWatcher>,
    last_used: Instant,
}

fn registry() -> &'static Mutex<HashMap<PathBuf, Watched>> {
    static R: OnceLock<Mutex<HashMap<PathBuf, Watched>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

fn canon(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

fn start_os_watcher(
    root: &Path,
    journal: Arc<Mutex<WriteJournal>>,
    arm_delay_ms: i64,
) -> Option<notify::RecommendedWatcher> {
    use notify::{EventKind, RecursiveMode, Watcher};
    let base = root.to_path_buf();
    let j = journal.clone();
    let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let at = now();
        let mut j = j.lock().unwrap();
        match res {
            Err(_) => j.mark_rescan(at),
            Ok(ev) => {
                if ev.need_rescan() {
                    j.mark_rescan(at);
                }
                if matches!(ev.kind, EventKind::Access(_)) {
                    j.heartbeat(at);
                    return;
                }
                for p in &ev.paths {
                    let rel = p
                        .strip_prefix(&base)
                        .map(|r| r.to_string_lossy().into_owned())
                        .unwrap_or_else(|_| p.to_string_lossy().into_owned());
                    j.record(rel, at);
                }
                if ev.paths.is_empty() {
                    // A write with no path: nothing is known about what changed.
                    j.record(".", at);
                }
            }
        }
    })
    .ok()?;
    w.watch(root, RecursiveMode::Recursive).ok()?;
    journal.lock().unwrap().arm(now() + arm_delay_ms);
    Some(w)
}

/// The journal covering `root`, starting an OS watcher on first use (per `cfg.watcher`).
pub fn journal_for(root: &Path, cfg: &IntervalConfig) -> Option<Arc<Mutex<WriteJournal>>> {
    let root = canon(root);
    let mut reg = registry().lock().unwrap();
    let idle = Duration::from_secs(cfg.idle_drop_secs.max(1));
    reg.retain(|_, w| w.kind == "manual" || w.last_used.elapsed() < idle);
    if let Some(w) = reg.get_mut(&root) {
        w.last_used = Instant::now();
        w.journal
            .lock()
            .unwrap()
            .prune_before(now() - JOURNAL_KEEP_MS);
        return Some(w.journal.clone());
    }
    if cfg.watcher == "none" {
        return None;
    }
    if reg.len() >= MAX_WATCHED
        && let Some(oldest) = reg
            .iter()
            .filter(|(_, w)| w.kind != "manual")
            .min_by_key(|(_, w)| w.last_used)
            .map(|(k, _)| k.clone())
    {
        reg.remove(&oldest);
    }
    let journal = Arc::new(Mutex::new(WriteJournal::new(cfg.max_events)));
    let watcher = start_os_watcher(&root, journal.clone(), cfg.arm_delay_ms as i64);
    watcher.as_ref()?;
    reg.insert(
        root,
        Watched {
            journal: journal.clone(),
            kind: "os",
            _watcher: watcher,
            last_used: Instant::now(),
        },
    );
    Some(journal)
}

/// Test/fake backend: register an armed journal for `root` that no OS watcher feeds. Use
/// [`feed`] to add writes.
pub fn install_manual(
    root: &Path,
    armed_at_ms: i64,
    max_events: usize,
) -> Arc<Mutex<WriteJournal>> {
    let root = canon(root);
    let mut j = WriteJournal::new(max_events);
    j.arm(armed_at_ms);
    let journal = Arc::new(Mutex::new(j));
    registry().lock().unwrap().insert(
        root,
        Watched {
            journal: journal.clone(),
            kind: "manual",
            _watcher: None,
            last_used: Instant::now(),
        },
    );
    journal
}

/// Test/fake backend: a write observed under `root` at `at_ms` (path relative to `root`).
pub fn feed(root: &Path, rel: &str, at_ms: i64) {
    if let Some(w) = registry().lock().unwrap().get(&canon(root)) {
        w.journal.lock().unwrap().record(rel, at_ms);
    }
}

/// Forget the journal of `root` (tests).
pub fn remove_watcher(root: &Path) {
    registry().lock().unwrap().remove(&canon(root));
}

// ---- state capture helpers ------------------------------------------------------------------

fn repo_root_of(dir: &Path) -> Option<PathBuf> {
    let id = subject::repo_identity(dir).ok()?;
    Some(canon(Path::new(&id.root)))
}

/// Which of `paths` Git ignores (`git check-ignore -z --stdin`). A failure ignores nothing: the
/// writes then count, which can only make a command unbound.
pub fn ignored_paths(root: &Path, paths: &[String]) -> HashSet<String> {
    use std::io::Write;
    let paths = &paths[..paths.len().min(MAX_IGNORE_PATHS)];
    if paths.is_empty() {
        return HashSet::new();
    }
    let Ok(mut child) = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["check-ignore", "-z", "--stdin"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return HashSet::new();
    };
    let mut input = Vec::new();
    for p in paths {
        input.extend_from_slice(p.as_bytes());
        input.push(0);
    }
    let mut stdin = child.stdin.take();
    let writer = std::thread::spawn(move || {
        if let Some(mut s) = stdin.take() {
            let _ = s.write_all(&input);
        }
    });
    let out = child.wait_with_output();
    let _ = writer.join();
    let Ok(out) = out else {
        return HashSet::new();
    };
    out.stdout
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

/// Other live runs working in the checkout `root` (not `run`), as display names.
fn other_writers(server: &Server, run: &str, root: &Path) -> Vec<String> {
    server.with_core(|c| {
        c.model
            .runs
            .iter()
            .filter(|r| r.id != run && r.ended_at_ms.is_none())
            .filter(|r| {
                matches!(
                    r.execution.value,
                    Execution::Working | Execution::Starting | Execution::RateLimited
                )
            })
            .filter(|r| {
                r.cwd
                    .as_deref()
                    .is_some_and(|d| canon(Path::new(d)).starts_with(root))
            })
            .map(|r| format!("run {}", r.name.clone().unwrap_or_else(|| r.handle.clone())))
            .collect()
    })
}

fn tasks_for_run(server: &Server, run: &str) -> BTreeSet<String> {
    server.with_core(|c| {
        c.store
            .load::<TaskRunBinding>(tracking::K_BINDING)
            .unwrap_or_default()
            .into_iter()
            .filter(|b| b.run_id == run && b.state == BindingState::Active)
            .map(|b| b.task_id)
            .collect()
    })
}

fn rec_id(run: &str, item: &str) -> String {
    format!("{run}:{item}")
}

fn load_rec(server: &Server, id: &str) -> Option<IntervalRec> {
    server.with_core(|c| c.store.find::<IntervalRec>(K_INTERVAL, id).ok().flatten())
}

// ---- begin / end ----------------------------------------------------------------------------

/// Start of a command's interval: arm the watcher, capture the checkout state, store the open
/// record. Blocking (git). `None` when the directory is not in a Git checkout.
pub fn begin(
    server: &Arc<Server>,
    cfg: &IntervalConfig,
    run: &str,
    item: &str,
    command: Option<&str>,
    cwd: &Path,
    start_ms: i64,
) -> Option<IntervalRec> {
    let root = repo_root_of(cwd)?;
    let journal = journal_for(&root, cfg);
    let (start_state, start_error) = match capture_code_state(&root) {
        Ok(s) => (Some(s), None),
        Err(e) => (None, Some(e.to_string())),
    };
    let watcher = match &journal {
        None => "none",
        Some(_) => registry()
            .lock()
            .unwrap()
            .get(&root)
            .map(|w| w.kind)
            .unwrap_or("none"),
    };
    let rec = IntervalRec {
        id: rec_id(run, item),
        run: run.into(),
        item: item.into(),
        command: command.map(|c| c.chars().take(2000).collect::<String>()),
        repo_root: root.to_string_lossy().into_owned(),
        start_ms,
        end_ms: None,
        start_state,
        start_error,
        other_writers: other_writers(server, run, &root),
        binding: None,
        watcher: watcher.into(),
        tools: BTreeMap::new(),
        tools_probed: BTreeSet::new(),
        tools_error: None,
    };
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.m.put(K_INTERVAL, &rec.id, None, &rec);
    let _ = server.commit(&mut c, tx);
    Some(rec)
}

/// End of a command's interval: settle, capture the end state, decide, store the closed
/// record and refresh the review packages the command's run feeds. Blocking.
pub fn end(
    server: &Arc<Server>,
    cfg: &IntervalConfig,
    run: &str,
    item: &str,
    end_ms: i64,
    wait_for_settle: bool,
) -> Option<IntervalBinding> {
    let mut rec = load_rec(server, &rec_id(run, item))?;
    if rec.binding.is_some() {
        return rec.binding;
    }
    if wait_for_settle && cfg.settle_ms > 0 {
        std::thread::sleep(Duration::from_millis(cfg.settle_ms));
    }
    let root = PathBuf::from(&rec.repo_root);
    let end_state = capture_code_state(&root).map_err(|e| e.to_string());
    let mut writers = rec.other_writers.clone();
    for w in other_writers(server, run, &root) {
        if !writers.contains(&w) {
            writers.push(w);
        }
    }
    let journal = registry().lock().unwrap().get_mut(&canon(&root)).map(|w| {
        w.last_used = Instant::now();
        w.journal.clone()
    });
    let window_end = end_ms.saturating_add(cfg.settle_ms as i64);
    // Ask Git about the written paths without holding the journal (the watcher feeds it).
    let written: Vec<String> = journal
        .as_ref()
        .map(|j| j.lock().unwrap().paths_in(rec.start_ms, window_end))
        .unwrap_or_default()
        .into_iter()
        .filter(|p| !iv::is_git_internal(p))
        .collect();
    let ignored = ignored_paths(&root, &written);
    let binding = {
        let empty = WriteJournal::default();
        let guard = journal.as_ref().map(|j| j.lock().unwrap());
        let j = guard.as_deref().unwrap_or(&empty);
        let start_state = match (&rec.start_state, &rec.start_error) {
            (Some(s), _) => Ok(s),
            (None, e) => Err(e.clone().unwrap_or_else(|| "not captured".into())),
        };
        let input = IntervalInput {
            start_ms: Some(rec.start_ms),
            end_ms: Some(end_ms),
            start_state,
            end_state: end_state.as_ref().map_err(Clone::clone),
            settle_ms: cfg.settle_ms as i64,
            other_writers: writers.clone(),
        };
        iv::decide(&input, j, &|p: &str| ignored.contains(p), now())
    };
    if binding.is_bound()
        && let Some(cmd) = rec.command.clone()
    {
        (rec.tools, rec.tools_probed, rec.tools_error) = probe_tools(&cmd, &root);
    }
    rec.end_ms = Some(end_ms);
    rec.other_writers = writers;
    rec.binding = Some(binding.clone());
    {
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.m.close(K_INTERVAL, &rec.id, None, &rec);
        let _ = server.commit(&mut c, tx);
    }
    for t in tasks_for_run(server, run) {
        spawn_refresh(server, &t);
    }
    Some(binding)
}

// ---- the tool-signal hook -------------------------------------------------------------------

enum Job {
    Begin {
        server: Arc<Server>,
        cfg: IntervalConfig,
        run: String,
        item: String,
        command: Option<String>,
        cwd: PathBuf,
        start_ms: i64,
    },
    End {
        server: Arc<Server>,
        cfg: IntervalConfig,
        run: String,
        item: String,
        end_ms: i64,
    },
}

fn worker() -> &'static mpsc::SyncSender<Job> {
    static TX: OnceLock<mpsc::SyncSender<Job>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = mpsc::sync_channel::<Job>(256);
        let _ = std::thread::Builder::new()
            .name("vk-interval".into())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    match job {
                        Job::Begin {
                            server,
                            cfg,
                            run,
                            item,
                            command,
                            cwd,
                            start_ms,
                        } => {
                            let _ = begin(
                                &server,
                                &cfg,
                                &run,
                                &item,
                                command.as_deref(),
                                &cwd,
                                start_ms,
                            );
                        }
                        Job::End {
                            server,
                            cfg,
                            run,
                            item,
                            end_ms,
                        } => {
                            let _ = end(&server, &cfg, &run, &item, end_ms, true);
                        }
                    }
                }
            });
        tx
    })
}

/// In unit tests the hook stays off unless a test turns it on (the existing review tests drive
/// `tracking::observe` and must not start real watchers).
static HOOK_IN_TESTS: AtomicBool = AtomicBool::new(false);

pub fn enable_hook_in_tests(on: bool) {
    HOOK_IN_TESTS.store(on, Ordering::SeqCst);
}

/// Hook from `tracking::observe`: a shell tool of a run that is bound to a task starts or ends.
/// Queues the work on the interval worker; never blocks the caller.
pub fn on_tool_signal(server: &Arc<Server>, run: &AgentRun, event: &str, p: &Value) {
    if cfg!(test) && !HOOK_IN_TESTS.load(Ordering::SeqCst) {
        return;
    }
    if !matches!(event, "PreToolUse" | "PostToolUse" | "PostToolUseFailure") {
        return;
    }
    let tool = p.get("tool_name").and_then(Value::as_str).unwrap_or("");
    let input = p.get("tool_input").cloned().unwrap_or(Value::Null);
    let Some(command) = tracking::shell_command(tool, &input) else {
        // The end event of a command carries no input; it is matched by its id below.
        return end_if_open(server, run, event, p);
    };
    let Some(call) = p.get("tool_use_id").and_then(Value::as_str) else {
        return;
    };
    if event != "PreToolUse" {
        return end_if_open(server, run, event, p);
    }
    let cfg = IntervalConfig::load();
    if !cfg.enabled || !cfg.harnesses.contains(&run.harness) {
        return;
    }
    if tasks_for_run(server, &run.id).is_empty() {
        return;
    }
    let cwd = input
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .or_else(|| run.cwd.as_deref().map(PathBuf::from));
    let Some(cwd) = cwd else { return };
    let id = rec_id(&run.id, call);
    if worker()
        .try_send(Job::Begin {
            server: server.clone(),
            cfg,
            run: run.id.clone(),
            item: call.to_string(),
            command: Some(command),
            cwd,
            start_ms: now(),
        })
        .is_ok()
    {
        queued().lock().unwrap().insert(id);
    }
}

/// Commands whose begin job was queued and whose end has not been seen yet.
fn queued() -> &'static Mutex<HashSet<String>> {
    static Q: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    Q.get_or_init(Default::default)
}

fn end_if_open(server: &Arc<Server>, run: &AgentRun, event: &str, p: &Value) {
    if event == "PreToolUse" {
        return;
    }
    let Some(call) = p.get("tool_use_id").and_then(Value::as_str) else {
        return;
    };
    if !queued().lock().unwrap().remove(&rec_id(&run.id, call)) {
        return;
    }
    // The worker runs jobs in order, so the begin job has finished before this one starts.
    let _ = worker().try_send(Job::End {
        server: server.clone(),
        cfg: IntervalConfig::load(),
        run: run.id.clone(),
        item: call.to_string(),
        end_ms: now(),
    });
}

// ---- review package integration -------------------------------------------------------------

fn key(run: &str, id: &str) -> String {
    format!("{run}:{id}")
}

/// Name the selected subject on every observed command whose execution interval was bound to
/// exactly that subject. Returns the decided interval records, by `<run>:<item>`.
pub fn apply(
    server: &Server,
    observed: &mut [ObservedCommand],
    subject: Option<&ChangeSubject>,
) -> Intervals {
    let runs: BTreeSet<&str> = observed.iter().map(|o| o.run_id.as_str()).collect();
    let mut found: Intervals = HashMap::new();
    for run in runs {
        let recs: Vec<IntervalRec> = server.with_core(|c| {
            c.store
                .load_by_field(K_INTERVAL, "$.run", run)
                .unwrap_or_default()
        });
        for r in recs {
            if r.binding.is_some() {
                found.insert(r.id.clone(), r);
            }
        }
    }
    if let Some(s) = subject {
        for o in observed.iter_mut() {
            if o.category != checks::CommandCategory::Observed || o.subject_id.is_some() {
                continue;
            }
            if let Some(b) = found
                .get(&key(&o.run_id, &o.id))
                .and_then(|r| r.binding.as_ref())
                && let Some(id) = b.subject_for(s)
            {
                o.subject_id = Some(id);
            }
        }
    }
    found
}

/// Decided interval records by `<run>:<item>`.
pub type Intervals = HashMap<String, IntervalRec>;

/// The environment identity of a bound command as the mapped check would record it (same hash
/// as a run's environment manifest: os, arch, runner and the versions of the tools in the
/// check's command), from the tool versions probed when the command ended. `None` when the
/// command is not bound, or any tool's identity was unavailable: freshness then stays unknown.
pub fn environment_for(
    found: &Intervals,
    cmd: &ObservedCommand,
    def_command: &CheckCommand,
) -> Option<String> {
    cmd.subject_id.as_ref()?;
    let rec = found.get(&key(&cmd.run_id, &cmd.id))?;
    if rec.tools_error.is_some() || rec.tools.is_empty() && rec.tools_probed.is_empty() {
        return None;
    }
    let mut tools = BTreeMap::new();
    for t in command_tools(def_command) {
        if !rec.tools_probed.contains(&t) {
            return None;
        }
        if let Some(v) = rec.tools.get(&t) {
            tools.insert(t, v.clone());
        }
    }
    Some(checks::EnvironmentManifest::new("host", None, tools, vec![]).digest)
}

/// Add an `interval` object to every row of `json["observed_commands"]`.
pub fn annotate(json: &mut Value, found: &Intervals) {
    let Some(rows) = json
        .get_mut("observed_commands")
        .and_then(Value::as_array_mut)
    else {
        return;
    };
    for row in rows {
        let (run, id) = (
            row.get("run_id").and_then(Value::as_str).unwrap_or(""),
            row.get("id").and_then(Value::as_str).unwrap_or(""),
        );
        let k = key(run, id);
        let v = match found.get(&k).and_then(|r| r.binding.as_ref()) {
            Some(b) => binding_json(b),
            None => json!({
                "status": "not_collected",
                "reasons": [],
                "explanation": [
                    "No execution-interval record for this command; code binding unverified",
                    iv::OFFER,
                ],
            }),
        };
        row["interval"] = v;
    }
}

pub fn binding_json(b: &IntervalBinding) -> Value {
    json!({
        "status": match b.status { IntervalStatus::Bound => "bound", IntervalStatus::Unbound => "unbound" },
        "reasons": b.reasons.iter().map(reason_json).collect::<Vec<_>>(),
        "explanation": b.explanation(),
        "state": b.state,
        "window_start_ms": b.window_start_ms,
        "window_end_ms": b.window_end_ms,
        "decided_at_ms": b.decided_at_ms,
    })
}

fn reason_json(r: &UnboundReason) -> Value {
    let mut v = serde_json::to_value(r).unwrap_or(Value::Null);
    v["text"] = json!(r.text());
    v
}

/// Part of the acceptance state token: decided intervals of the task's runs (a newly bound
/// command changes what a package would say).
pub fn token_part(c: &Core, task: &str) -> String {
    let mut ids: Vec<String> = Vec::new();
    for b in bindings_of(c, task) {
        let recs: Vec<IntervalRec> = c
            .store
            .load_by_field(K_INTERVAL, "$.run", &b.run_id)
            .unwrap_or_default();
        ids.extend(
            recs.into_iter()
                .filter(|r| r.binding.is_some())
                .map(|r| r.id),
        );
    }
    ids.sort();
    format!("intervals {ids:?}\n")
}

// ---- API ------------------------------------------------------------------------------------

pub(super) fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    Some(match method {
        "task.review.intervals" => intervals_api(server, ctx, p),
        "task.review.interval_status" => Ok(status_json()),
        _ => return None,
    })
}

fn intervals_api(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let task_id = req(p, "task")?;
    authorize_task(server, ctx, task_id)?;
    let task = tracking::find_task(server, task_id)?;
    let limit = u(p, "limit").unwrap_or(200).clamp(1, 1000) as usize;
    let mut recs: Vec<IntervalRec> = server.with_core(|c| {
        let mut v = Vec::new();
        for b in bindings_of(c, &task.id) {
            v.extend(
                c.store
                    .load_by_field::<IntervalRec>(K_INTERVAL, "$.run", &b.run_id)
                    .unwrap_or_default(),
            );
        }
        v
    });
    recs.sort_by_key(|r| (std::cmp::Reverse(r.start_ms), r.id.clone()));
    recs.dedup_by(|a, b| a.id == b.id);
    recs.truncate(limit);
    let intervals: Vec<Value> = recs
        .iter()
        .map(|r| {
            json!({
                "id": r.id,
                "run": r.run,
                "item": r.item,
                "command": r.command,
                "repo_root": r.repo_root,
                "start_ms": r.start_ms,
                "end_ms": r.end_ms,
                "watcher": r.watcher,
                "start_state": r.start_state.as_ref().map(CodeState::label),
                "start_error": r.start_error,
                "other_writers": r.other_writers,
                "decided": r.binding.is_some(),
                "binding": r.binding.as_ref().map(binding_json),
            })
        })
        .collect();
    Ok(json!({"task": task.id, "intervals": intervals}))
}

fn status_json() -> Value {
    let cfg = IntervalConfig::load();
    let reg = registry().lock().unwrap();
    let mut checkouts: Vec<Value> = reg
        .iter()
        .map(|(root, w)| {
            let j = w.journal.lock().unwrap();
            json!({
                "root": root.to_string_lossy(),
                "watcher": w.kind,
                "armed": j.is_armed(),
                "armed_at_ms": j.armed_at_ms(),
                "events": j.len(),
                "gaps": j.gap_count(),
                "idle_s": w.last_used.elapsed().as_secs(),
            })
        })
        .collect();
    checkouts.sort_by(|a, b| a["root"].as_str().cmp(&b["root"].as_str()));
    json!({
        "enabled": cfg.enabled,
        "watcher": cfg.watcher,
        "settle_ms": cfg.settle_ms,
        "arm_delay_ms": cfg.arm_delay_ms,
        "harnesses": cfg.harnesses,
        "checkouts": checkouts,
        "note": "A command is bound to a subject only when a watcher armed before it started saw no relevant write during it and the checkout state at its start and end match. Only harnesses whose pre-tool hook runs before the tool are collected.",
    })
}
