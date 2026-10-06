//! Session desk (research R2, spec 07 `desk.*`): find and reopen previous work.
//!
//! A conversation index separate from scrollback search: an incremental indexer reads the
//! native transcripts of runs Vibeke has seen (`AgentRun.transcript_path`), plus — only when
//! the user opts in under `[desk] roots` — transcript directories per harness, and stores
//! (machine, harness, native session, repo/cwd, turn, role/kind, text, timestamp, source
//! offset) rows in `desk.db` (FTS5). Indexing runs on a blocking thread with its own SQLite
//! connection: it never holds the state lock while reading files or writing the index, and
//! each pass is bounded by a byte budget.
//!
//! Opening a result focuses the live run's pane only on an explicit request (`focus: true`);
//! resuming is a separate explicit call (`desk.resume`) that distinguishes **Resume native
//! session** (the harness's own resume) from **Start new agent with context** (a reviewable
//! context package saved as a draft; nothing is ever sent automatically).

use crate::Server;
use crate::api::{Ctx, R, err, internal, invalid, not_found, req, s, u};
use crate::core::Tx;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use vk_proto::model::*;
use vk_proto::rpc::{ErrorKind, RpcError};
use vk_store::conv::{ConvIndex, ConvQuery, ConvRow, ConvSource};

pub const METHODS: &[(&str, bool)] = &[
    ("desk.search", false),
    ("desk.sessions", false),
    ("desk.context", false),
    ("desk.open", true),
    ("desk.resume", true),
    ("desk.forget", true),
    ("desk.status", false),
    ("desk.index", true),
];

/// Largest text kept per indexed item.
const MAX_ROW_BYTES: usize = 8192;
/// Most bytes read from one transcript per pass (a pass continues where it stopped).
const PER_FILE_BYTES: u64 = 4 << 20;
/// Bytes a search may index first so that just-finished turns are findable.
const CATCH_UP_BYTES: u64 = 2 << 20;
/// Largest context package text.
const MAX_PACKAGE_BYTES: usize = 32 * 1024;

/// `[desk]` in config.toml.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DeskConfig {
    /// Index the transcripts of runs Vibeke has seen (default on).
    pub index: bool,
    /// Extra transcript directories per harness (`claude = ["~/.claude/projects"]`). Default
    /// empty: nothing outside Vibeke's own runs is read unless the user opts in.
    pub roots: BTreeMap<String, Vec<String>>,
    /// Never index transcripts whose path, cwd or repository is under one of these
    /// (`/path` = that directory and below; a trailing `*` matches any prefix).
    pub exclude: Vec<String>,
    /// Indexed rows older than this are pruned.
    pub retention_days: u32,
    pub interval_s: u64,
    pub pass_bytes: u64,
    pub max_root_files: usize,
}

impl Default for DeskConfig {
    fn default() -> Self {
        DeskConfig {
            index: true,
            roots: BTreeMap::new(),
            exclude: vec![],
            retention_days: 90,
            interval_s: 15,
            pass_bytes: 8 << 20,
            max_root_files: 5000,
        }
    }
}

impl DeskConfig {
    pub fn from_config(cfg: &vk_config::Config) -> Self {
        cfg.extra
            .get("desk")
            .and_then(|t| serde_json::to_value(t).ok())
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PassStats {
    pub at_ms: i64,
    pub registered: u64,
    pub bytes: u64,
    pub rows: u64,
    pub purged: u64,
    pub reset: u64,
    /// Sources with bytes left to read after this pass.
    pub pending: u64,
    pub errors: Vec<String>,
}

#[derive(Default)]
pub struct State {
    index: Mutex<Option<ConvIndex>>,
    last: Mutex<Option<PassStats>>,
    /// Tests set the configuration directly instead of through config.toml.
    pub config_override: Mutex<Option<DeskConfig>>,
    passes: AtomicU64,
    last_prune_ms: AtomicI64,
}

pub fn config(server: &Server) -> DeskConfig {
    if let Some(c) = server.desk.config_override.lock().unwrap().clone() {
        return c;
    }
    vk_config::Config::load(vk_config::config_path())
        .map(|(c, _)| DeskConfig::from_config(&c))
        .unwrap_or_default()
}

pub fn db_path(server: &Server) -> PathBuf {
    server.paths.state.join("desk.db")
}

/// Run `f` on the index from a blocking thread (never under the state lock).
async fn with_index<T: Send + 'static>(
    server: &Arc<Server>,
    f: impl FnOnce(&mut ConvIndex) -> anyhow::Result<T> + Send + 'static,
) -> Result<T, RpcError> {
    let srv = server.clone();
    tokio::task::spawn_blocking(move || {
        let mut g = srv.desk.index.lock().unwrap();
        let ix = open(&srv, &mut g)?;
        f(ix)
    })
    .await
    .map_err(internal)?
    .map_err(internal)
}

fn open<'a>(server: &Server, g: &'a mut Option<ConvIndex>) -> anyhow::Result<&'a mut ConvIndex> {
    if g.is_none() {
        *g = Some(ConvIndex::open(&db_path(server))?);
    }
    Ok(g.as_mut().unwrap())
}

// ---- parsing ------------------------------------------------------------------------------

/// Result of parsing a chunk of transcript bytes.
#[derive(Debug, Default)]
pub struct Parsed {
    pub rows: Vec<ConvRow>,
    /// Bytes up to and including the last complete line.
    pub consumed: u64,
    pub turns: u32,
    pub session: Option<String>,
    pub cwd: Option<String>,
    pub first_ts: Option<i64>,
    pub last_ts: Option<i64>,
}

fn bounded(t: &str, max: usize) -> String {
    if t.len() <= max {
        return t.to_string();
    }
    let mut end = max;
    while !t.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &t[..end])
}

fn text_of(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

/// Parse complete JSONL lines of a Claude or Codex transcript starting at byte `base`, with
/// `turns` turns already seen. Turn numbering matches `agent.transcript` (`n`): a user prompt
/// (not a tool result) starts a turn; items before the first prompt belong to turn 1.
pub fn parse_chunk(buf: &[u8], base: u64, turns: u32, fallback_ts: i64) -> Parsed {
    let end = buf
        .iter()
        .rposition(|&b| b == b'\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let mut out = Parsed {
        consumed: end as u64,
        turns,
        ..Default::default()
    };
    let mut off = 0usize;
    for line in buf[..end].split_inclusive(|&b| b == b'\n') {
        let line_off = base + off as u64;
        off += line.len();
        let Ok(v) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        // Native session identity and cwd: Claude lines carry `sessionId`/`cwd`; Codex
        // rollouts start with `session_meta` and repeat the cwd in `turn_context`.
        if out.session.is_none() {
            out.session = v
                .get("sessionId")
                .and_then(Value::as_str)
                .or_else(|| {
                    (v.get("type").and_then(Value::as_str) == Some("session_meta"))
                        .then(|| v.pointer("/payload/id").and_then(Value::as_str))
                        .flatten()
                })
                .map(str::to_string);
        }
        if out.cwd.is_none() {
            out.cwd = v
                .get("cwd")
                .or_else(|| v.pointer("/payload/cwd"))
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        let ts = v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(crate::agents::usage::parse_rfc3339_ms)
            .unwrap_or(fallback_ts);
        let (prompt, items) = crate::gateway_api::line_items(&v);
        if items.is_empty() {
            continue;
        }
        if prompt || out.turns == 0 {
            out.turns += 1;
        }
        for it in items {
            let kind = it["kind"].as_str().unwrap_or("");
            let (role, text) = match kind {
                "text" => (
                    it["role"].as_str().unwrap_or("").to_string(),
                    text_of(&it["text"]),
                ),
                "tool_call" => (
                    "tool".into(),
                    Some(format!(
                        "{}: {}",
                        it["tool"].as_str().unwrap_or("tool"),
                        it["summary"].as_str().unwrap_or("")
                    )),
                ),
                "tool_result" => ("tool".into(), text_of(&it["summary"])),
                // Model reasoning is not indexed.
                _ => continue,
            };
            let Some(text) = text.filter(|t| !t.trim().is_empty()) else {
                continue;
            };
            out.rows.push(ConvRow {
                turn: out.turns,
                role,
                kind: kind.into(),
                text: bounded(&text, MAX_ROW_BYTES),
                ts,
                offset: line_off,
            });
            out.first_ts = Some(out.first_ts.map_or(ts, |f| f.min(ts)));
            out.last_ts = Some(out.last_ts.map_or(ts, |l| l.max(ts)));
        }
    }
    out
}

// ---- source selection -----------------------------------------------------------------------

fn expand(p: &str) -> String {
    match p.strip_prefix("~/") {
        Some(rest) => crate::paths::home()
            .join(rest)
            .to_string_lossy()
            .into_owned(),
        None if p == "~" => crate::paths::home().to_string_lossy().into_owned(),
        None => p.to_string(),
    }
}

fn under(pattern: &str, x: &str) -> bool {
    let pat = expand(pattern);
    if let Some(pre) = pat.strip_suffix('*') {
        return x.starts_with(pre);
    }
    let pat = pat.trim_end_matches('/');
    x == pat || x.starts_with(&format!("{pat}/"))
}

/// Excluded by `[desk] exclude` (transcript path, session cwd or repository).
pub fn excluded(cfg: &DeskConfig, path: &str, cwd: Option<&str>, repo: Option<&str>) -> bool {
    cfg.exclude.iter().any(|pat| {
        [Some(path), cwd, repo]
            .into_iter()
            .flatten()
            .any(|x| under(pat, x))
    })
}

fn root_allowed(cfg: &DeskConfig, s: &ConvSource) -> bool {
    cfg.roots
        .get(&s.harness)
        .is_some_and(|dirs| dirs.iter().any(|d| under(d, &s.path)))
}

struct Candidate {
    path: String,
    harness: String,
    session: Option<String>,
    cwd: Option<String>,
    run: Option<String>,
    workspace: Option<String>,
    origin: &'static str,
}

/// Transcripts of live runs (and, when `closed`, ended runs) that are not yet registered as
/// run sources. Holds the state lock only to copy run fields.
fn run_candidates(server: &Server, known_runs: &HashSet<String>, closed: bool) -> Vec<Candidate> {
    server.with_core(|c| {
        let mut runs: Vec<AgentRun> = c.model.runs.clone();
        if closed {
            runs.extend(
                c.store
                    .load_closed::<AgentRun>("run", 500)
                    .unwrap_or_default(),
            );
        }
        runs.into_iter()
            .filter_map(|r| {
                let path = r.transcript_path.clone()?;
                if known_runs.contains(&path) {
                    return None;
                }
                let workspace = c.pane(&r.pane).map(|p| p.workspace.clone()).or_else(|| {
                    c.store
                        .find::<Pane>("pane", &r.pane)
                        .ok()
                        .flatten()
                        .map(|p| p.workspace)
                });
                Some(Candidate {
                    path,
                    harness: r.harness.clone(),
                    session: r.harness_session_id.clone(),
                    cwd: r.cwd.clone(),
                    run: Some(r.id.clone()),
                    workspace,
                    origin: "run",
                })
            })
            .collect()
    })
}

fn walk(dir: &Path, depth: u32, out: &mut Vec<PathBuf>, max: usize) {
    if depth == 0 || out.len() >= max {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        if out.len() >= max {
            return;
        }
        let Ok(ft) = e.file_type() else { continue };
        let p = e.path();
        if ft.is_dir() {
            walk(&p, depth - 1, out, max);
        } else if ft.is_file() && p.extension().is_some_and(|x| x == "jsonl") {
            out.push(p);
        }
    }
}

fn root_candidates(cfg: &DeskConfig) -> Vec<Candidate> {
    let mut out = vec![];
    for (harness, dirs) in &cfg.roots {
        for d in dirs {
            let mut files = vec![];
            walk(Path::new(&expand(d)), 6, &mut files, cfg.max_root_files);
            out.extend(files.into_iter().map(|p| Candidate {
                path: p.to_string_lossy().into_owned(),
                harness: harness.clone(),
                session: None,
                cwd: None,
                run: None,
                workspace: None,
                origin: "root",
            }));
        }
    }
    out
}

fn repo_of(cache: &mut HashMap<String, Option<String>>, cwd: &str) -> Option<String> {
    cache
        .entry(cwd.to_string())
        .or_insert_with(|| {
            vk_tasks::repo_root(Path::new(cwd)).map(|r| r.root.to_string_lossy().into_owned())
        })
        .clone()
}

fn mtime_ms(m: &std::fs::Metadata) -> i64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn read_at(path: &str, offset: u64, len: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::with_capacity(len as usize);
    f.take(len).read_to_end(&mut buf)?;
    Ok(buf)
}

/// One bounded indexing pass (blocking): register new sources, read new bytes of each known
/// source up to `budget`, drop excluded sources, apply retention.
pub fn index_pass(server: &Server, budget: u64) -> anyhow::Result<PassStats> {
    let cfg = config(server);
    let st = &server.desk;
    let mut stats = PassStats {
        at_ms: vk_store::now_ms(),
        ..Default::default()
    };
    let mut g = st.index.lock().unwrap();
    let ix = open(server, &mut g)?;
    let forgotten = ix.forgotten()?;
    let known: HashMap<String, ConvSource> = ix
        .sources()?
        .into_iter()
        .map(|s| (s.path.clone(), s))
        .collect();
    let known_runs: HashSet<String> = known
        .values()
        .filter(|s| s.origin == "run")
        .map(|s| s.path.clone())
        .collect();
    let n = st.passes.fetch_add(1, Ordering::Relaxed);
    let mut cands = vec![];
    if cfg.index {
        // Ended runs are rescanned occasionally (they were usually registered while live).
        cands.extend(run_candidates(server, &known_runs, n.is_multiple_of(20)));
    }
    cands.extend(root_candidates(&cfg));
    let mut repos: HashMap<String, Option<String>> = HashMap::new();
    for c in cands {
        if c.session.as_ref().is_some_and(|s| forgotten.contains(s)) {
            continue;
        }
        let is_new = !known.contains_key(&c.path);
        let mut s = known.get(&c.path).cloned().unwrap_or_else(|| ConvSource {
            path: c.path.clone(),
            machine: server.opts.machine.clone(),
            harness: c.harness.clone(),
            origin: c.origin.into(),
            ..Default::default()
        });
        let before = s.clone();
        if c.origin == "run" {
            s.origin = "run".into();
            s.harness = c.harness;
            s.run = c.run.or(s.run.take());
            s.workspace = c.workspace.or(s.workspace.take());
        }
        if s.session.is_none() {
            s.session = c.session;
        }
        if s.cwd.is_none() {
            s.cwd = c.cwd;
        }
        if s.repo.is_none()
            && let Some(cwd) = s.cwd.clone()
        {
            s.repo = repo_of(&mut repos, &cwd);
        }
        if is_new || s != before {
            ix.put(&s)?;
            stats.registered += 1;
        }
    }
    let mut left = budget;
    for mut s in ix.sources()? {
        let selected = match s.origin.as_str() {
            "run" => cfg.index,
            _ => root_allowed(&cfg, &s),
        };
        if !selected || excluded(&cfg, &s.path, s.cwd.as_deref(), s.repo.as_deref()) {
            stats.purged += ix.remove_source(&s.path)? as u64;
            continue;
        }
        if s.session.as_ref().is_some_and(|x| forgotten.contains(x)) {
            continue;
        }
        let Ok(meta) = std::fs::metadata(&s.path) else {
            continue;
        };
        let (size, mtime) = (meta.len(), mtime_ms(&meta));
        if size < s.offset {
            // Truncated or replaced: index it again from the start.
            ix.reset(&s.path)?;
            s.offset = 0;
            s.turns = 0;
            s.rows = 0;
            s.first_ts = None;
            s.last_ts = None;
            stats.reset += 1;
        }
        if size == s.offset {
            if s.size != size || s.mtime_ms != mtime {
                s.size = size;
                s.mtime_ms = mtime;
                ix.put(&s)?;
            }
            continue;
        }
        if left == 0 {
            stats.pending += 1;
            continue;
        }
        let want = (size - s.offset).min(left).min(PER_FILE_BYTES);
        let buf = match read_at(&s.path, s.offset, want) {
            Ok(b) => b,
            Err(e) => {
                stats.errors.push(format!("{}: {e}", s.path));
                continue;
            }
        };
        let mut p = parse_chunk(&buf, s.offset, s.turns, mtime);
        if p.consumed == 0 && buf.len() as u64 >= PER_FILE_BYTES {
            // One line longer than the per-pass cap: skip it rather than stall.
            p.consumed = buf.len() as u64;
        }
        if s.session.is_none() {
            s.session = p.session.clone().or_else(|| {
                Path::new(&s.path)
                    .file_stem()
                    .map(|x| x.to_string_lossy().into_owned())
            });
        }
        if s.cwd.is_none() {
            s.cwd = p.cwd.clone();
        }
        if s.repo.is_none()
            && let Some(cwd) = s.cwd.clone()
        {
            s.repo = repo_of(&mut repos, &cwd);
        }
        if excluded(&cfg, &s.path, s.cwd.as_deref(), s.repo.as_deref()) {
            stats.purged += ix.remove_source(&s.path)? as u64;
            continue;
        }
        let skip_rows = s.session.as_ref().is_some_and(|x| forgotten.contains(x));
        let rows = if skip_rows { vec![] } else { p.rows };
        s.offset += p.consumed;
        s.turns = p.turns;
        s.size = size;
        s.mtime_ms = mtime;
        if !rows.is_empty() {
            s.first_ts = match (s.first_ts, p.first_ts) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            s.last_ts = s.last_ts.max(p.last_ts);
        }
        s.rows += rows.len() as u64;
        s.indexed_at = Some(vk_store::now_ms());
        ix.append(&s, &rows)?;
        stats.rows += rows.len() as u64;
        stats.bytes += p.consumed;
        left = left.saturating_sub(p.consumed.max(1));
        if s.offset < size {
            stats.pending += 1;
        }
    }
    let now = vk_store::now_ms();
    if now - st.last_prune_ms.load(Ordering::Relaxed) > 3_600_000 {
        st.last_prune_ms.store(now, Ordering::Relaxed);
        stats.purged += ix.prune(now - cfg.retention_days as i64 * 86_400_000)? as u64;
    }
    drop(g);
    *st.last.lock().unwrap() = Some(stats.clone());
    Ok(stats)
}

async fn catch_up(server: &Arc<Server>, budget: u64) {
    let srv = server.clone();
    let _ = tokio::task::spawn_blocking(move || index_pass(&srv, budget)).await;
}

/// Background indexer: a bounded pass every `interval_s`.
pub fn start(server: &Arc<Server>) {
    let srv = server.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(2)).await;
        loop {
            let cfg = config(&srv);
            let s2 = srv.clone();
            match tokio::task::spawn_blocking(move || index_pass(&s2, cfg.pass_bytes)).await {
                Ok(Err(e)) => tracing::warn!(error = %e, "desk index pass failed"),
                Err(e) => tracing::warn!(error = %e, "desk index pass panicked"),
                Ok(Ok(_)) => {}
            }
            crate::drafts::prune(&srv);
            tokio::time::sleep(Duration::from_secs(cfg.interval_s.max(2))).await;
        }
    });
}

// ---- session status ---------------------------------------------------------------------

/// Live (a current run is in that native session), resumable (resume argv known) or neither.
#[derive(Debug, Clone, Serialize)]
pub struct SessionStatus {
    pub status: &'static str,
    pub run: Option<String>,
    pub pane: Option<String>,
    pub resume_argv: Vec<String>,
    pub resume_command: Option<String>,
    /// Stored run whose resume handle would be used.
    pub resume_run: Option<String>,
}

fn status_of(server: &Server, sessions: &[(String, String)]) -> HashMap<String, SessionStatus> {
    let mut out = HashMap::new();
    let mut need_manifest = vec![];
    server.with_core(|c| {
        for (sid, harness) in sessions {
            if out.contains_key(sid) {
                continue;
            }
            if let Some(r) = c
                .model
                .runs
                .iter()
                .find(|r| r.harness_session_id.as_deref() == Some(sid.as_str()))
            {
                out.insert(
                    sid.clone(),
                    SessionStatus {
                        status: "live",
                        run: Some(r.id.clone()),
                        pane: Some(r.pane.clone()),
                        resume_argv: vec![],
                        resume_command: None,
                        resume_run: None,
                    },
                );
                continue;
            }
            let stored = c
                .store
                .load_by_field::<AgentRun>("run", "$.harness_session_id", sid)
                .unwrap_or_default()
                .into_iter()
                .rev()
                .find(|r| !r.resume_argv.is_empty());
            match stored {
                Some(r) => {
                    out.insert(
                        sid.clone(),
                        SessionStatus {
                            status: "resumable",
                            run: None,
                            pane: None,
                            resume_command: Some(crate::agents::harness::shell_join(
                                &r.resume_argv,
                            )),
                            resume_argv: r.resume_argv,
                            resume_run: Some(r.id),
                        },
                    );
                }
                None => need_manifest.push((sid.clone(), harness.clone())),
            }
        }
    });
    // Harness resume argv may consult manifests on disk: outside the state lock.
    for (sid, harness) in need_manifest {
        let argv = crate::agents::Harness::from_id(&harness)
            .map(|h| h.resume_argv(&sid))
            .unwrap_or_default();
        out.insert(
            sid,
            SessionStatus {
                status: if argv.is_empty() { "none" } else { "resumable" },
                run: None,
                pane: None,
                resume_command: (!argv.is_empty())
                    .then(|| crate::agents::harness::shell_join(&argv)),
                resume_argv: argv,
                resume_run: None,
            },
        );
    }
    out
}

// ---- params -----------------------------------------------------------------------------

/// `since`/`until`: epoch ms, `7d`/`12h`/`30m` ago, or an RFC 3339 date/time.
pub(crate) fn time_param(p: &Value, k: &str) -> Result<Option<i64>, RpcError> {
    let Some(v) = p.get(k).filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    if let Some(n) = v.as_i64() {
        return Ok(Some(n));
    }
    let t = v.as_str().unwrap_or("").trim();
    let unit = |c: char| match c {
        'd' => Some(86_400_000),
        'h' => Some(3_600_000),
        'm' => Some(60_000),
        _ => None,
    };
    if let Some(last) = t.chars().last()
        && let Some(ms) = unit(last)
        && let Ok(n) = t[..t.len() - 1].parse::<i64>()
    {
        return Ok(Some(vk_store::now_ms() - n * ms));
    }
    let full = if t.len() == 10 {
        format!("{t}T00:00:00Z")
    } else {
        t.to_string()
    };
    crate::agents::usage::parse_rfc3339_ms(&full)
        .map(Some)
        .ok_or_else(|| {
            invalid(format!(
                "`{k}`: expected epoch ms, 7d/12h/30m or YYYY-MM-DD"
            ))
        })
}

/// `repo`: a path; resolved to its main repository root when it is inside a git repository.
fn repo_param(p: &Value) -> Option<String> {
    let r = s(p, "repo")?;
    let abs = std::fs::canonicalize(r)
        .map(|x| x.to_string_lossy().into_owned())
        .unwrap_or_else(|_| r.to_string());
    Some(
        vk_tasks::repo_root(Path::new(&abs))
            .map(|i| i.root.to_string_lossy().into_owned())
            .unwrap_or(abs),
    )
}

// ---- API ----------------------------------------------------------------------------------

pub async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    if !method.starts_with("desk.") {
        return None;
    }
    let scope = match crate::drafts::caller_ws(server, ctx, method) {
        Ok(s) => s,
        Err(e) => return Some(Err(e)),
    };
    Some(match method {
        "desk.search" => search(server, scope, p).await,
        "desk.sessions" => sessions(server, scope, p).await,
        "desk.context" => context(server, scope, p).await,
        "desk.open" => open_session(server, ctx, p).await,
        "desk.resume" => resume(server, ctx, p).await,
        "desk.forget" => forget(server, p).await,
        "desk.status" => status(server).await,
        "desk.index" => {
            let budget = u(p, "budget").unwrap_or(config(server).pass_bytes);
            let srv = server.clone();
            tokio::task::spawn_blocking(move || index_pass(&srv, budget))
                .await
                .map_err(internal)
                .and_then(|r| r.map_err(internal))
                .map(|st| json!({"pass": st}))
        }
        _ => return None,
    })
}

async fn search(server: &Arc<Server>, scope: Option<String>, p: &Value) -> R {
    let text = req(p, "text")?.to_string();
    let q = ConvQuery {
        text,
        repo: repo_param(p),
        harness: s(p, "harness").map(str::to_string),
        since_ms: time_param(p, "since")?,
        until_ms: time_param(p, "until")?,
        session: s(p, "session").map(str::to_string),
        workspace: scope,
        recent: s(p, "sort") == Some("recent"),
        limit: u(p, "limit").unwrap_or(20).clamp(1, 200) as usize,
    };
    if p.get("fresh").and_then(Value::as_bool) != Some(false) {
        catch_up(server, CATCH_UP_BYTES).await;
    }
    let (hits, stats) = with_index(server, move |ix| Ok((ix.search(&q)?, ix.stats()?))).await?;
    let keys: Vec<(String, String)> = hits
        .iter()
        .map(|h| (h.session.clone(), h.harness.clone()))
        .collect();
    let st = status_of(server, &keys);
    let hits: Vec<Value> = hits
        .into_iter()
        .map(|h| {
            let mut v = serde_json::to_value(&h).unwrap_or_default();
            if let Some(x) = st.get(&h.session) {
                v["status"] = json!(x.status);
                v["live"] = json!({"run": x.run, "pane": x.pane});
                v["resume"] = json!({"argv": x.resume_argv, "command": x.resume_command, "run": x.resume_run});
            }
            v
        })
        .collect();
    Ok(json!({
        "hits": hits,
        "index": {"sources": stats.0, "rows": stats.1, "forgotten_sessions": stats.2},
    }))
}

fn matches_repo(repo: &str, s_repo: Option<&str>, cwd: Option<&str>) -> bool {
    s_repo == Some(repo) || cwd.is_some_and(|c| under(repo, c))
}

async fn sessions(server: &Arc<Server>, scope: Option<String>, p: &Value) -> R {
    if p.get("fresh").and_then(Value::as_bool) != Some(false) {
        catch_up(server, CATCH_UP_BYTES).await;
    }
    let repo = repo_param(p);
    let harness = s(p, "harness").map(str::to_string);
    let limit = u(p, "limit").unwrap_or(50).clamp(1, 500) as usize;
    let list = with_index(server, |ix| ix.sessions()).await?;
    let list: Vec<_> = list
        .into_iter()
        .filter(|x| scope.is_none() || x.workspace == scope)
        .filter(|x| harness.as_ref().is_none_or(|h| &x.harness == h))
        .filter(|x| {
            repo.as_ref()
                .is_none_or(|r| matches_repo(r, x.repo.as_deref(), x.cwd.as_deref()))
        })
        .take(limit)
        .collect();
    let keys: Vec<(String, String)> = list
        .iter()
        .map(|x| (x.session.clone(), x.harness.clone()))
        .collect();
    let st = status_of(server, &keys);
    let out: Vec<Value> = list
        .into_iter()
        .map(|x| {
            let mut v = serde_json::to_value(&x).unwrap_or_default();
            if let Some(t) = st.get(&x.session) {
                v["status"] = json!(t.status);
                v["live"] = json!({"run": t.run, "pane": t.pane});
                v["resume"] = json!({"argv": t.resume_argv, "command": t.resume_command, "run": t.resume_run});
            }
            v
        })
        .collect();
    Ok(json!({"sessions": out}))
}

/// The indexed session (pane-scoped callers: only their workspace's).
async fn find_session(
    server: &Arc<Server>,
    scope: Option<&str>,
    id: &str,
) -> Result<Option<vk_store::conv::ConvSession>, RpcError> {
    let id2 = id.to_string();
    let found = with_index(server, move |ix| {
        Ok(ix.sessions()?.into_iter().find(|x| x.session == id2))
    })
    .await?;
    if let Some(sc) = scope
        && found
            .as_ref()
            .is_some_and(|x| x.workspace.as_deref() != Some(sc))
    {
        return Err(not_found("session", id));
    }
    Ok(found)
}

/// Session metadata from the index or, if not indexed yet, from a live run.
async fn session_meta(
    server: &Arc<Server>,
    scope: Option<&str>,
    id: &str,
) -> Result<(String, Option<String>, Option<String>, Option<String>), RpcError> {
    if let Some(x) = find_session(server, scope, id).await? {
        return Ok((x.harness, x.cwd, x.workspace, x.paths.last().cloned()));
    }
    let run = server.with_core(|c| {
        c.model
            .runs
            .iter()
            .find(|r| r.harness_session_id.as_deref() == Some(id))
            .map(|r| {
                (
                    r.harness.clone(),
                    r.cwd.clone(),
                    c.pane(&r.pane).map(|p| p.workspace.clone()),
                )
            })
    });
    match run {
        Some((h, cwd, ws)) if scope.is_none() || ws.as_deref() == scope => Ok((h, cwd, ws, None)),
        _ => Err(not_found("session", id)),
    }
}

async fn open_session(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let id = req(p, "session")?.to_string();
    let (harness, cwd, workspace, last_path) = session_meta(server, None, &id).await?;
    let st = status_of(server, &[(id.clone(), harness.clone())])
        .remove(&id)
        .expect("status for the requested session");
    let turn = u(p, "turn").map(|t| t as u32);
    let path = s(p, "path").map(str::to_string).or(last_path);
    let items = match turn {
        Some(t) => {
            let (id2, path2) = (id.clone(), path.clone());
            with_index(server, move |ix| ix.turns(&id2, path2.as_deref(), t, t))
                .await?
                .into_iter()
                .map(|(_, r)| json!({"role": r.role, "kind": r.kind, "text": bounded(&r.text, 2000), "ts": r.ts, "offset": r.offset}))
                .collect::<Vec<_>>()
        }
        None => vec![],
    };
    // Focusing is an explicit user action: only with `focus: true`, only for a live run.
    let mut focused = false;
    if p.get("focus").and_then(Value::as_bool) == Some(true)
        && let Some(pane) = &st.pane
    {
        server.focus_pane(&ctx.client_id, pane);
        focused = true;
    }
    let action = match st.status {
        "live" => "focus_live_pane",
        "resumable" => "resume_native_session",
        _ => "start_new_agent_with_context",
    };
    Ok(json!({
        "session": id, "harness": harness, "cwd": cwd, "workspace": workspace,
        "path": path, "turn": turn, "turn_items": items,
        "status": st.status, "live": {"run": st.run, "pane": st.pane}, "focused": focused,
        "action": action,
        "actions": [
            {"id": "focus", "label": "Focus live pane", "available": st.status == "live"},
            {"id": "resume_native", "label": "Resume native session", "available": st.status == "resumable", "command": st.resume_command, "argv": st.resume_argv, "method": "desk.resume"},
            {"id": "new_agent", "label": "Start new agent with context", "available": true, "method": "desk.resume", "params": {"mode": "new_agent"}, "note": "builds an editable context package as a draft; nothing is sent"},
        ],
    }))
}

// ---- context package --------------------------------------------------------------------

fn turn_list(p: &Value) -> Option<Vec<u32>> {
    match p.get("turns")? {
        Value::Array(a) => Some(
            a.iter()
                .filter_map(|v| v.as_u64())
                .map(|n| n as u32)
                .collect(),
        ),
        Value::String(s) => {
            let mut v = vec![];
            for part in s.split(',') {
                match part.split_once('-') {
                    Some((a, b)) => {
                        let (a, b): (u32, u32) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
                        v.extend(a..=b);
                    }
                    None => v.push(part.trim().parse().ok()?),
                }
            }
            Some(v)
        }
        Value::Number(n) => n.as_u64().map(|n| vec![n as u32]),
        _ => None,
    }
}

fn git_out(cwd: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn extract(lines: &[String], keys: &[&str], max: usize) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for l in lines {
        let low = l.to_lowercase();
        if keys.iter().any(|k| low.contains(k)) {
            let t = bounded(l.trim().trim_start_matches(['-', '*', ' ']), 300);
            if !t.is_empty() && !out.contains(&t) {
                out.push(t);
            }
        }
        if out.len() >= max {
            break;
        }
    }
    out
}

/// Build the reviewable context package for **Start new agent with context**: objective,
/// decisions, remaining work, selected turns and repository revision, as editable text. The
/// decision/remaining lists are deterministic line extracts, labelled for the user to edit.
async fn context(server: &Arc<Server>, scope: Option<String>, p: &Value) -> R {
    let id = req(p, "session")?.to_string();
    let x = find_session(server, scope.as_deref(), &id)
        .await?
        .ok_or_else(|| not_found("session", &id))?;
    let path = s(p, "path")
        .map(str::to_string)
        .or_else(|| x.paths.last().cloned());
    let (id2, path2) = (id.clone(), path.clone());
    let rows = with_index(server, move |ix| {
        ix.turns(&id2, path2.as_deref(), 1, u32::MAX)
    })
    .await?;
    let mut turns: BTreeMap<u32, Vec<ConvRow>> = BTreeMap::new();
    for (_, r) in rows {
        turns.entry(r.turn).or_default().push(r);
    }
    let last = turns.keys().last().copied().unwrap_or(0);
    let selected: Vec<u32> = turn_list(p)
        .unwrap_or_else(|| (last.saturating_sub(2).max(1)..=last).collect())
        .into_iter()
        .filter(|n| turns.contains_key(n))
        .collect();
    let first_prompt = turns
        .values()
        .flatten()
        .find(|r| r.role == "user" && r.kind == "text")
        .map(|r| bounded(&r.text, 2000));
    let objective = s(p, "objective")
        .map(str::to_string)
        .or(first_prompt)
        .unwrap_or_default();
    let assistant_lines: Vec<String> = turns
        .values()
        .flatten()
        .filter(|r| r.role == "assistant")
        .flat_map(|r| r.text.lines().map(str::to_string).collect::<Vec<_>>())
        .collect();
    let decisions = extract(
        &assistant_lines,
        &[
            "decided",
            "decision",
            "we'll go with",
            "chose ",
            "going with",
        ],
        10,
    );
    let remaining = extract(
        &assistant_lines,
        &[
            "todo",
            "next step",
            "remaining",
            "still need",
            "not yet",
            "follow-up",
            "follow up",
        ],
        10,
    );
    let cwd = x.cwd.clone();
    let revision = match cwd.clone() {
        Some(cwd) if Path::new(&cwd).is_dir() => tokio::task::spawn_blocking(move || {
            let rev = git_out(&cwd, &["rev-parse", "HEAD"])?;
            let branch = git_out(&cwd, &["rev-parse", "--abbrev-ref", "HEAD"]);
            let dirty = git_out(&cwd, &["status", "--porcelain"])
                .map(|s| s.lines().count())
                .unwrap_or(0);
            Some(json!({"revision": rev, "branch": branch, "uncommitted_files": dirty}))
        })
        .await
        .ok()
        .flatten(),
        _ => None,
    };
    let sel: Vec<Value> = selected
        .iter()
        .map(|n| {
            let rs = &turns[n];
            let user: Vec<String> = rs
                .iter()
                .filter(|r| r.role == "user" && r.kind == "text")
                .map(|r| bounded(&r.text, 4000))
                .collect();
            let assistant = rs
                .iter()
                .rev()
                .find(|r| r.role == "assistant" && r.kind == "text")
                .map(|r| bounded(&r.text, 4000));
            let tools = rs.iter().filter(|r| r.kind == "tool_call").count();
            json!({"turn": n, "user": user, "assistant": assistant, "tool_calls": tools})
        })
        .collect();
    let mut text = format!(
        "# Context from a previous {} session ({})\n\nThis is not a native resume: the new agent sees only the text below. Review and edit it before sending.\n\n## Objective\n{}\n\n",
        x.harness,
        id,
        if objective.is_empty() {
            "(describe the objective)"
        } else {
            &objective
        }
    );
    let bullets = |v: &[String], empty: &str| {
        if v.is_empty() {
            format!("- {empty}\n")
        } else {
            v.iter().map(|l| format!("- {l}\n")).collect::<String>()
        }
    };
    text.push_str("## Decisions (extracted lines; edit)\n");
    text.push_str(&bullets(
        &decisions,
        "(none found; add the decisions that matter)",
    ));
    text.push_str("\n## Remaining work (extracted lines; edit)\n");
    text.push_str(&bullets(&remaining, "(none found; list what is left)"));
    text.push_str("\n## Repository\n");
    match (&x.repo, &revision) {
        (repo, Some(r)) => text.push_str(&format!(
            "{} at {} {} ({} uncommitted files); cwd {}\n",
            repo.as_deref().unwrap_or("(repo)"),
            r["branch"].as_str().unwrap_or("?"),
            r["revision"].as_str().unwrap_or("?"),
            r["uncommitted_files"],
            cwd.as_deref().unwrap_or("?")
        )),
        (repo, None) => text.push_str(&format!(
            "{} (revision unavailable); cwd {}\n",
            repo.as_deref().unwrap_or("(no repository)"),
            cwd.as_deref().unwrap_or("?")
        )),
    }
    text.push_str("\n## Selected turns\n");
    for t in &sel {
        text.push_str(&format!("\n### Turn {}\n", t["turn"]));
        for u in t["user"].as_array().into_iter().flatten() {
            text.push_str(&format!("User: {}\n", u.as_str().unwrap_or("")));
        }
        if let Some(a) = t["assistant"].as_str() {
            text.push_str(&format!("Assistant: {a}\n"));
        }
    }
    let text = bounded(&text, MAX_PACKAGE_BYTES);
    let package = json!({
        "session": id, "harness": x.harness, "path": path,
        "objective": objective, "decisions": decisions, "remaining_work": remaining,
        "selected_turns": sel, "repository": {"repo": x.repo, "cwd": cwd, "revision": revision},
    });
    let v = json!({
        "label": "Start new agent with context",
        "package": package,
        "text": text,
        "note": "a reviewable package, not a native resume; nothing is sent",
    });
    Ok(v)
}

// ---- resume -------------------------------------------------------------------------------

fn template_run(harness: &str, session: &str, argv: Vec<String>, cwd: Option<String>) -> AgentRun {
    let t = vk_store::now_ms();
    AgentRun {
        id: format!("desk:{session}"),
        handle: String::new(),
        name: None,
        pane: String::new(),
        harness: harness.into(),
        harness_version: None,
        integration: "process".into(),
        harness_session_id: Some(session.into()),
        transcript_path: None,
        resume_argv: argv,
        cwd,
        model: None,
        task: None,
        execution: Facet {
            value: Execution::Unknown,
            since_ms: t,
            source: StateSource::Process,
            confidence: 0.5,
            detail: None,
        },
        health: AdapterHealth::Healthy,
        yolo: false,
        permission_mode: None,
        last_message: None,
        last_tool: None,
        turns_completed: 0,
        done_rev: 0,
        started_at_ms: t,
        ended_at_ms: None,
        capabilities: vec![],
        usage: Default::default(),
        rate_limit: None,
    }
}

/// A free pane for a resumed/new agent: the given pane (must have no live run) or a new tab in
/// the session's workspace (else the caller's), at the session cwd when it still exists.
async fn target_pane(
    server: &Arc<Server>,
    ctx: &Ctx,
    p: &Value,
    workspace: Option<&str>,
    cwd: Option<&str>,
) -> Result<String, RpcError> {
    if let Some(t) = s(p, "pane") {
        let pane = crate::api::resolve_pane(server, ctx, Some(t))?;
        if server.with_core(|c| c.run_for_pane(&pane.id).is_some()) {
            return Err(crate::tracking::conflict(
                "pane_busy",
                "that pane already has an agent; choose a free pane or omit `pane`",
            ));
        }
        return Ok(pane.id);
    }
    let ws = match s(p, "workspace") {
        Some(w) => crate::api::resolve_ws(server, ctx, Some(w))?.id,
        None => match workspace.filter(|w| server.with_core(|c| c.ws(w).is_some())) {
            Some(w) => w.to_string(),
            None => crate::api::resolve_ws(server, ctx, None)
                .map(|w| w.id)
                .or_else(|_| {
                    server
                        .with_core(|c| c.model.workspaces.first().map(|w| w.id.clone()))
                        .ok_or_else(|| invalid("no workspace"))
                })?,
        },
    };
    let cwd = cwd.filter(|c| Path::new(c).is_dir());
    let (_, pane) = server
        .create_tab(&ws, cwd, None, None, None)
        .map_err(internal)?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    Ok(pane.id)
}

async fn resume(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let id = req(p, "session")?.to_string();
    let (harness, cwd, workspace, _) = session_meta(server, None, &id).await?;
    match s(p, "mode").unwrap_or("native") {
        "native" => {
            let st = status_of(server, &[(id.clone(), harness.clone())])
                .remove(&id)
                .expect("status for the requested session");
            match st.status {
                "live" => Err(crate::tracking::conflict(
                    "session_live",
                    "this session is running now; focus its pane instead (desk.open --focus)",
                )
                .details(json!({"reason": "session_live", "pane": st.pane, "run": st.run}))),
                "resumable" => {
                    let run = match &st.resume_run {
                        Some(r) => server
                            .with_core(|c| c.store.find::<AgentRun>("run", r).ok().flatten()),
                        None => None,
                    };
                    let explicit_pane = s(p, "pane").is_some() || s(p, "workspace").is_some();
                    let (run, pane) = match run {
                        // A stored run keeps agent.resume's behaviour (its old pane if free).
                        Some(r) if !explicit_pane => (r, None),
                        r => {
                            let pane = target_pane(
                                server,
                                ctx,
                                p,
                                workspace.as_deref(),
                                cwd.as_deref(),
                            )
                            .await?;
                            let run = r.unwrap_or_else(|| {
                                template_run(&harness, &id, st.resume_argv.clone(), cwd.clone())
                            });
                            (run, Some(pane))
                        }
                    };
                    let v = crate::agents::resume_from(server, run, pane).await?;
                    Ok(json!({"mode": "native", "label": "Resume native session", "command": st.resume_command, "run": v["run"]}))
                }
                _ => Err(err(
                    ErrorKind::Unsupported,
                    format!("no native resume handle for this {harness} session"),
                )
                .details(json!({"alternative": {"mode": "new_agent", "label": "Start new agent with context"}}))),
            }
        }
        "new_agent" => {
            let pkg = context(server, None, p).await?;
            let text = pkg["text"].as_str().unwrap_or("").to_string();
            let ws = match s(p, "workspace") {
                Some(w) => crate::api::resolve_ws(server, ctx, Some(w))?.id,
                None => match workspace.filter(|w| server.with_core(|c| c.ws(w).is_some())) {
                    Some(w) => w,
                    None => crate::api::resolve_ws(server, ctx, None)?.id,
                },
            };
            let short: String = id.chars().take(8).collect();
            let draft = crate::drafts::create_internal(
                server,
                ctx,
                "workspace",
                &ws,
                &ws,
                Some(format!("Context from {harness} session {short}")),
                &text,
            )?;
            let mut out = json!({
                "mode": "new_agent", "label": "Start new agent with context",
                "draft": draft, "package": pkg["package"], "sent": false,
                "note": "Nothing was sent. Review and edit the draft, then send it with draft.send.",
            });
            if p.get("start").and_then(Value::as_bool) == Some(true) {
                let h = s(p, "harness").unwrap_or(&harness).to_string();
                let pane = target_pane(server, ctx, p, Some(&ws), cwd.as_deref()).await?;
                let v =
                    crate::agents::start_in_pane(server, &pane, &h, None, None, &[], None).await?;
                out["run"] = v.get("run").cloned().unwrap_or(v);
                out["pane"] = json!(pane);
            }
            Ok(out)
        }
        other => Err(invalid(format!(
            "mode `{other}`: expected native (Resume native session) or new_agent (Start new agent with context)"
        ))),
    }
}

// ---- forget / status --------------------------------------------------------------------

async fn forget(server: &Arc<Server>, p: &Value) -> R {
    let session = s(p, "session").map(str::to_string);
    let repo = repo_param(p);
    let ws = s(p, "workspace").map(|w| {
        crate::api::resolve_ws(server, &crate::drafts::user_ctx(), Some(w))
            .map(|w| w.id)
            .unwrap_or_else(|_| w.to_string())
    });
    let before = time_param(p, "before")?;
    if session.is_none() && repo.is_none() && ws.is_none() && before.is_none() {
        return Err(invalid(
            "desk.forget needs session, repo, workspace or before",
        ));
    }
    let scope = json!({"session": session, "repo": repo, "workspace": ws, "before": before});
    let (rows, tombstoned) = with_index(server, move |ix| {
        let now = vk_store::now_ms();
        let mut sessions: Vec<String> = session.into_iter().collect();
        if let Some(r) = &repo {
            sessions.extend(ix.sessions_in_repo(r)?);
        }
        if let Some(w) = &ws {
            sessions.extend(ix.sessions_in_workspace(w)?);
        }
        sessions.sort();
        sessions.dedup();
        let mut rows = 0;
        for x in &sessions {
            rows += ix.forget_session(x, true, now)?;
        }
        if let Some(b) = before {
            rows += ix.prune(b)?;
        }
        Ok((rows, sessions))
    })
    .await?;
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    // Metadata only: which scope was purged and how many rows; never conversation text.
    tx.event(
        "desk.forgotten",
        json!({"scope": scope}),
        json!({"rows": rows, "sessions": tombstoned.len()}),
    );
    server.commit(&mut c, tx).map_err(internal)?;
    drop(c);
    // Derived assistant results and cached excerpts for this scope go too (14 §8).
    crate::assist::forget_scope(server, &scope);
    Ok(json!({"rows_deleted": rows, "sessions_forgotten": tombstoned}))
}

async fn status(server: &Arc<Server>) -> R {
    let cfg = config(server);
    let cfg2 = cfg.clone();
    let (stats, sources) = with_index(server, move |ix| {
        let srcs: Vec<Value> = ix
            .sources()?
            .into_iter()
            .take(200)
            .map(|s| {
                let size = std::fs::metadata(&s.path).map(|m| m.len()).ok();
                json!({
                    "path": s.path, "harness": s.harness, "session": s.session, "origin": s.origin,
                    "repo": s.repo, "rows": s.rows, "offset": s.offset, "size": size,
                    "pending_bytes": size.map(|z| z.saturating_sub(s.offset)),
                    "excluded": excluded(&cfg2, &s.path, s.cwd.as_deref(), s.repo.as_deref()),
                })
            })
            .collect();
        Ok((ix.stats()?, srcs))
    })
    .await?;
    let last = server.desk.last.lock().unwrap().clone();
    Ok(json!({
        "db": db_path(server),
        "selection": {
            "runs": cfg.index,
            "roots": cfg.roots,
            "note": "only transcripts of runs Vibeke has seen are indexed unless [desk] roots opts in",
        },
        "exclude": cfg.exclude,
        "retention_days": cfg.retention_days,
        "counts": {"sources": stats.0, "rows": stats.1, "forgotten_sessions": stats.2},
        "sources": sources,
        "last_pass": last,
    }))
}

#[cfg(test)]
#[path = "desk_tests.rs"]
mod tests;
