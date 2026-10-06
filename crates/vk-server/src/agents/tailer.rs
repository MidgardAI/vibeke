//! TranscriptTailer (04 §3.1, §10): an incremental, `notify`-driven tail of each live run's
//! transcript with offset tracking.
//!
//! Parsing is [`vk_agents::transcript`]; this module owns the files, the watcher and what the
//! events do:
//!
//! - **Per-turn records.** A finished turn becomes a compact `turn_usage` record (tokens, model,
//!   cost) keyed by its native id, so a re-read, a second tailer pass or the structured `Stop`
//!   path never writes it twice, and an `agent.turn_usage` event. Cost is the harness-reported
//!   figure, else the price table ([`super::usage::cost_for`]); subscription-billed runs keep
//!   tokens and no dollars.
//! - **Run totals.** The parser's running totals refresh `AgentRun.usage` (the same numbers the
//!   `Stop` re-read computes), so a hooks-only run that misses a `Stop` still converges.
//! - **Items and reconcile.** A tool call the structured transport did not report becomes a
//!   `tool_item` record; a `tool_result` for a `tool_use_id` that still has an open interaction
//!   resolves it (the tool ran, so the approval was answered somewhere): the reconcile path for
//!   hooks-only runs, and what the arbiter calls after structured loss ([`reconcile_state`]).
//! - **Search.** Transcript text is indexed in the desk's FTS index (`desk.db`) unless
//!   `search.index_transcripts = false`.
//!
//! Raw transcripts are never copied: Vibeke keeps pointers (`AgentRun.transcript_path`), offsets
//! and summaries.

use super::*;
use notify::{RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicI64, Ordering};
use vk_agents::transcript::{Activity, Event, Format, Parser};

/// Store kind of the compact per-turn usage records.
pub const K_TURN_USAGE: &str = "turn_usage";
/// Store kind shared with `tracking` for tool items (`<run>:<call id>`).
const K_ITEM: &str = "tool_item";

/// Bytes read from one file per poll (a poll continues where it stopped).
const CHUNK: u64 = 1 << 20;
/// Poll even without file-system events (missed events, files that did not exist yet).
const TICK: Duration = Duration::from_secs(5);
/// Tail window used by [`reconcile_state`].
const RECONCILE_WINDOW: u64 = 2 << 20;

/// One compact per-turn record (02 `turns`/`items`, 04 §10).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnUsageRecord {
    /// `<run>:<native id>`.
    pub id: String,
    pub run: String,
    pub harness: String,
    /// Position of the turn in its transcript.
    pub n: u32,
    pub native_id: String,
    pub model: Option<String>,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost_usd: Option<f64>,
    /// `harness` | `price_table` | `subscription` | `none`.
    pub cost_source: String,
    pub stop: Option<String>,
    pub ended_at_ms: Option<i64>,
    /// `transcript`.
    pub source: String,
}

struct Tail {
    run: String,
    harness: String,
    path: PathBuf,
    offset: u64,
    parser: Parser,
}

#[derive(Default)]
struct Registry {
    tails: HashMap<PathBuf, Tail>,
    watched: HashSet<PathBuf>,
}

static REG: LazyLock<Mutex<Registry>> = LazyLock::new(|| Mutex::new(Registry::default()));
static LAST_INDEX_MS: AtomicI64 = AtomicI64::new(0);

/// `search.index_transcripts` (default true).
pub fn index_enabled(cfg: &vk_config::Config) -> bool {
    cfg.extra
        .get("search")
        .and_then(|s| s.get("index_transcripts"))
        .and_then(toml::Value::as_bool)
        .unwrap_or(true)
}

fn format_for(h: Harness, path: &Path) -> Option<Format> {
    if let Some(l) = h.manifest() {
        let f = l.m.transcript.format.as_str();
        if let Some(fmt) = Format::parse(f) {
            return Some(fmt);
        }
        if f == "none" || f.starts_with("external:") {
            return None;
        }
    }
    let mut head = vec![0u8; 8192];
    use std::io::Read;
    let n = std::fs::File::open(path)
        .and_then(|mut f| f.read(&mut head))
        .ok()?;
    Format::sniff(&String::from_utf8_lossy(&head[..n]))
}

/// The run's transcript file: the path a hook reported, else the manifest's `[identity]`
/// `transcript_glob` for its session.
pub fn transcript_of(run: &AgentRun) -> Option<PathBuf> {
    if let Some(p) = &run.transcript_path {
        return Some(PathBuf::from(p));
    }
    let h = Harness::from_id(&run.harness)?;
    h.manifest()?.transcript_path(
        &crate::paths::home(),
        run.cwd.as_deref(),
        run.harness_session_id.as_deref(),
    )
}

/// Start tailing `run`'s transcript (idempotent; a changed path replaces the tail).
pub fn track(run: &AgentRun) -> bool {
    if run.ended_at_ms.is_some() {
        return false;
    }
    let Some(path) = transcript_of(run) else {
        return false;
    };
    let Some(h) = Harness::from_id(&run.harness) else {
        return false;
    };
    let mut reg = REG.lock().unwrap();
    if reg.tails.get(&path).is_some_and(|t| t.run == run.id) {
        return true;
    }
    // A file may not exist yet (the harness creates it at the first prompt): sniff later.
    let Some(fmt) = format_for(h, &path).or_else(|| {
        h.manifest()
            .filter(|l| {
                l.m.transcript.format != "none" && !l.m.transcript.format.starts_with("external:")
            })
            .and_then(|_| default_format(h))
    }) else {
        return false;
    };
    reg.tails.retain(|_, t| t.run != run.id);
    reg.tails.insert(
        path.clone(),
        Tail {
            run: run.id.clone(),
            harness: run.harness.clone(),
            path,
            offset: 0,
            parser: Parser::new(fmt),
        },
    );
    true
}

fn default_format(h: Harness) -> Option<Format> {
    match h.family() {
        harness::Family::Claude => Some(Format::ClaudeJsonl),
        harness::Family::Codex => Some(Format::CodexRollout),
        harness::Family::Pi => Some(Format::PiJsonl),
        harness::Family::Omp => Some(Format::OmpJsonl),
        _ => None,
    }
}

pub fn untrack(run: &str) {
    REG.lock().unwrap().tails.retain(|_, t| t.run != run);
}

/// Paths currently tailed: `(run, path, offset)`.
pub fn tailing() -> Vec<(String, PathBuf, u64)> {
    REG.lock()
        .unwrap()
        .tails
        .values()
        .map(|t| (t.run.clone(), t.path.clone(), t.offset))
        .collect()
}

fn read_from(path: &Path, offset: u64, max: u64) -> std::io::Result<(Vec<u8>, u64)> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    // Truncated or replaced: start over.
    let start = if len < offset { 0 } else { offset };
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    f.take(max).read_to_end(&mut buf)?;
    Ok((buf, start))
}

/// Read what each tail has not seen yet and apply the events. Returns the events applied.
pub fn poll_all(server: &Arc<Server>) -> usize {
    let work: Vec<(PathBuf, String, String, u64)> = {
        let reg = REG.lock().unwrap();
        reg.tails
            .values()
            .map(|t| (t.path.clone(), t.run.clone(), t.harness.clone(), t.offset))
            .collect()
    };
    let mut total = 0;
    for (path, run, harness, offset) in work {
        let Ok((bytes, start)) = read_from(&path, offset, CHUNK) else {
            continue;
        };
        let events = {
            let mut reg = REG.lock().unwrap();
            let Some(t) = reg.tails.get_mut(&path) else {
                continue;
            };
            if start != t.offset {
                // The file shrank: a new conversation in the same path.
                t.parser = Parser::new(t.parser.format());
            }
            t.offset = start + bytes.len() as u64;
            if bytes.is_empty() {
                continue;
            }
            let ev = t.parser.feed(&bytes);
            let totals = t.parser.totals().clone();
            let model = t.parser.model().map(str::to_string);
            (ev, totals, model)
        };
        let (ev, totals, model) = events;
        total += ev.len();
        apply(server, &run, &harness, &ev, &totals, model.as_deref());
    }
    if total > 0 {
        index_soon(server);
    }
    total
}

fn apply(
    server: &Arc<Server>,
    run_id: &str,
    harness: &str,
    events: &[Event],
    totals: &vk_agents::transcript::TurnUsage,
    model: Option<&str>,
) {
    let Some(run) = server.with_core(|c| c.run(run_id).cloned()) else {
        untrack(run_id);
        return;
    };
    let billing = super::usage::billing_of(harness);
    for e in events {
        match e {
            Event::Turn {
                id,
                n,
                usage,
                model: m,
                stop,
                ts_ms,
            } => {
                let rec_id = format!("{run_id}:{id}");
                let exists = server.with_core(|c| {
                    c.store
                        .find::<TurnUsageRecord>(K_TURN_USAGE, &rec_id)
                        .ok()
                        .flatten()
                        .is_some()
                });
                if exists {
                    continue;
                }
                let (cost, source) = super::usage::cost_for(
                    server,
                    billing.as_deref(),
                    m.as_deref().or(model),
                    usage,
                );
                let rec = TurnUsageRecord {
                    id: rec_id.clone(),
                    run: run_id.to_string(),
                    harness: harness.to_string(),
                    n: *n,
                    native_id: id.clone(),
                    model: m.clone().or_else(|| model.map(str::to_string)),
                    input: usage.input,
                    output: usage.output,
                    cache_read: usage.cache_read,
                    cache_write: usage.cache_write,
                    cost_usd: cost,
                    cost_source: source.to_string(),
                    stop: stop.clone(),
                    ended_at_ms: *ts_ms,
                    source: "transcript".into(),
                };
                server.with_core(|c| {
                    let mut tx = Tx::new();
                    tx.m.put(K_TURN_USAGE, &rec.id, None, &rec);
                    tx.event(
                        "agent.turn_usage",
                        json!({"run": run_id, "pane": run.pane}),
                        json!({"turn": rec.n, "native_id": rec.native_id, "model": rec.model, "input": rec.input, "output": rec.output, "cache_read": rec.cache_read, "cache_write": rec.cache_write, "cost_usd": rec.cost_usd, "cost_source": rec.cost_source, "source": "transcript"}),
                    );
                    let _ = c.commit(tx);
                });
            }
            Event::ToolUse {
                id,
                name,
                command,
                ts_ms,
            } => {
                let item = format!("{run_id}:{id}");
                let exists = server.with_core(|c| {
                    c.store
                        .find::<vk_review::checks::ToolRecord>(K_ITEM, &item)
                        .ok()
                        .flatten()
                        .is_some()
                });
                if exists {
                    continue;
                }
                let rec = vk_review::checks::ToolRecord {
                    run_id: run_id.to_string(),
                    turn: Some(run.turns_completed + 1),
                    item_id: id.clone(),
                    tool: name.clone(),
                    command: command.clone(),
                    cwd: run.cwd.clone(),
                    exit_code: None,
                    started_at_ms: Some(ts_ms.unwrap_or_else(now_ms)),
                    ended_at_ms: None,
                    source: vk_review::checks::ToolRecordSource::NativeTool,
                    text: None,
                    established_subject: None,
                };
                server.with_core(|c| {
                    let mut tx = Tx::new();
                    tx.m.put(K_ITEM, &item, None, &rec);
                    let _ = c.commit(tx);
                });
            }
            Event::ToolResult {
                id,
                is_error,
                ts_ms,
            } => {
                let item = format!("{run_id}:{id}");
                let rec = server.with_core(|c| {
                    c.store
                        .find::<vk_review::checks::ToolRecord>(K_ITEM, &item)
                        .ok()
                        .flatten()
                });
                if let Some(mut r) = rec
                    && r.ended_at_ms.is_none()
                {
                    r.ended_at_ms = Some(ts_ms.unwrap_or_else(now_ms));
                    // A shell tool the transcript says failed, with no exit code from a hook.
                    if *is_error && r.exit_code.is_none() && r.command.is_some() {
                        r.exit_code = None;
                    }
                    server.with_core(|c| {
                        let mut tx = Tx::new();
                        tx.m.close(K_ITEM, &item, None, &r);
                        let _ = c.commit(tx);
                    });
                }
                // Reconcile: the tool ran, so its approval is no longer pending.
                let open = server.with_core(|c| {
                    c.model
                        .interactions
                        .iter()
                        .find(|i| {
                            i.run == run_id
                                && i.status == InteractionStatus::Open
                                && i.native_ref.as_deref() == Some(id.as_str())
                        })
                        .map(|i| i.id.clone())
                });
                if let Some(i) = open {
                    resolve(
                        server,
                        &i,
                        InteractionStatus::ResolvedElsewhere,
                        "native:transcript tool_result",
                    );
                }
            }
            Event::TurnStarted { .. } => {}
        }
    }
    if !totals.is_empty() {
        super::usage::from_tailer(server, &run, totals, model, billing.as_deref());
    }
}

/// What the transcript says the run is doing (for the arbiter's `reconcile`): `Idle` when the
/// last turn ended, `Working` when a prompt or tool call is pending. `None` without a readable
/// transcript.
pub fn reconcile_state(_server: &Server, run: &AgentRun) -> Option<Execution> {
    let path = transcript_of(run)?;
    let h = Harness::from_id(&run.harness)?;
    let fmt = format_for(h, &path).or_else(|| default_format(h))?;
    let len = std::fs::metadata(&path).ok()?.len();
    let from = len.saturating_sub(RECONCILE_WINDOW);
    let (mut bytes, _) = read_from(&path, from, RECONCILE_WINDOW).ok()?;
    if from > 0
        && let Some(i) = bytes.iter().position(|b| *b == b'\n')
    {
        bytes.drain(..=i);
    }
    let mut p = Parser::new(fmt);
    p.feed(&bytes);
    match p.activity()? {
        Activity::Idle => Some(Execution::Idle),
        Activity::Working => Some(Execution::Working),
    }
}

/// Index transcript text for search, at most every 5 s and only with
/// `search.index_transcripts` (the desk indexer does the work, bounded per pass).
fn index_soon(server: &Arc<Server>) {
    let now = now_ms();
    let last = LAST_INDEX_MS.load(Ordering::Relaxed);
    if now - last < 5_000 {
        return;
    }
    let enabled = vk_config::Config::load(vk_config::config_path())
        .map(|(c, _)| index_enabled(&c))
        .unwrap_or(true);
    if !enabled {
        return;
    }
    LAST_INDEX_MS.store(now, Ordering::Relaxed);
    let srv = server.clone();
    let budget = crate::desk::config(server).pass_bytes;
    tokio::task::spawn_blocking(move || {
        let _ = crate::desk::index_pass(&srv, budget);
    });
}

/// Register tails for live runs and drop those of ended runs.
pub fn sync_runs(server: &Server) {
    let live: Vec<AgentRun> = server.with_core(|c| {
        c.model
            .runs
            .iter()
            .filter(|r| r.ended_at_ms.is_none() && !headless::is_headless(r))
            .cloned()
            .collect()
    });
    let ids: HashSet<&str> = live.iter().map(|r| r.id.as_str()).collect();
    REG.lock()
        .unwrap()
        .tails
        .retain(|_, t| ids.contains(t.run.as_str()));
    for r in &live {
        track(r);
    }
}

fn watch_dirs(watcher: &mut notify::RecommendedWatcher) {
    let dirs: Vec<PathBuf> = REG
        .lock()
        .unwrap()
        .tails
        .values()
        .filter_map(|t| t.path.parent().map(Path::to_path_buf))
        .collect();
    for d in dirs {
        let new = REG.lock().unwrap().watched.insert(d.clone());
        if new && d.is_dir() {
            let _ = watcher.watch(&d, RecursiveMode::NonRecursive);
        }
    }
}

/// Background task: a `notify` watcher on the transcripts' directories plus a 5 s poll.
pub fn start(server: &Arc<Server>) {
    let srv = server.clone();
    let wake = Arc::new(tokio::sync::Notify::new());
    let w2 = wake.clone();
    let watcher = notify::recommended_watcher(move |_res: notify::Result<notify::Event>| {
        w2.notify_one();
    });
    tokio::spawn(async move {
        let mut watcher = watcher.ok();
        loop {
            let s2 = srv.clone();
            let n = tokio::task::spawn_blocking(move || {
                sync_runs(&s2);
                poll_all(&s2)
            })
            .await
            .unwrap_or(0);
            if let Some(w) = watcher.as_mut() {
                watch_dirs(w);
            }
            // After a busy poll go again at once (more bytes may be waiting), else wait for an
            // fs event or the tick.
            if n == 0 {
                let _ = tokio::time::timeout(TICK, wake.notified()).await;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    });
}

/// Compact per-turn records of a run, oldest first.
pub fn records_of(server: &Server, run: &str) -> Vec<TurnUsageRecord> {
    server.with_core(|c| {
        let mut v: Vec<TurnUsageRecord> = c
            .store
            .load_by_field(K_TURN_USAGE, "$.run", run)
            .unwrap_or_default();
        v.sort_by_key(|r| r.n);
        v.dedup_by_key(|r| r.id.clone());
        v
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn index_flag_defaults_on_and_reads_the_search_section() {
        let on = vk_config::Config::default();
        assert!(index_enabled(&on));
        let mut off = vk_config::Config::default();
        off.extra.insert(
            "search".into(),
            toml::Value::Table(
                [("index_transcripts".to_string(), toml::Value::Boolean(false))]
                    .into_iter()
                    .collect(),
            ),
        );
        assert!(!index_enabled(&off));
    }

    #[test]
    fn read_from_resumes_at_the_offset_and_restarts_after_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.jsonl");
        std::fs::write(&p, "aaa\nbbb\n").unwrap();
        let (b, start) = read_from(&p, 0, 1024).unwrap();
        assert_eq!((b.as_slice(), start), (&b"aaa\nbbb\n"[..], 0));
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(b"ccc\n").unwrap();
        let (b, start) = read_from(&p, 8, 1024).unwrap();
        assert_eq!((b.as_slice(), start), (&b"ccc\n"[..], 8));
        std::fs::write(&p, "z\n").unwrap();
        let (b, start) = read_from(&p, 12, 1024).unwrap();
        assert_eq!(
            (b.as_slice(), start),
            (&b"z\n"[..], 0),
            "truncated: from the top"
        );
        // Bounded reads continue where they stopped.
        let (b, _) = read_from(&p, 0, 1).unwrap();
        assert_eq!(b.len(), 1);
    }
}
