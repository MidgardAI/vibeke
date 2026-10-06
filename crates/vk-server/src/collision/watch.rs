//! Signals 2 and 3 of the collision tracker (05 §10): the file-system watcher with attribution
//! and the `git status --porcelain` poll, plus the worker thread that drives both.
//!
//! Only a checkout that two or more live runs share (or that holds a claim of a live run) is
//! watched and polled. A change is never an agent's collision unless a run was working there:
//! attribution is (a) a run with an in-flight reported tool call on that path, (b) with
//! `fs_attribution = "aggressive"`, the run whose process held the file open for writing (Linux
//! `/proc/*/fd`, best effort: the writer has usually closed the file by the time the event
//! arrives), (c) the runs working in that checkout, `ambiguous` with more than one. A change an
//! adapter already reported (a certain touch of that path moments ago) is explained by that
//! report and adds nothing.
//!
//! The worker is one thread holding a `Weak<Server>`; [`tick`] is the deterministic body tests
//! call directly, with [`feed_fs`] standing in for the platform watcher (`watcher = "none"`).

use super::{IN_FLIGHT_GRACE_MS, InFlight, config, live_runs, record, root_of, run_alive, sweep};
use crate::Server;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use vk_proto::model::{AgentRun, Execution};
use vk_store::now_ms;
use vk_tasks::collision as vc;

/// Watched checkouts at once.
const MAX_WATCHED: usize = 16;
/// Watcher events waiting to settle, at most.
const MAX_PENDING: usize = 20_000;
/// Distinct paths asked of `git check-ignore` per flush.
const MAX_FLUSH_PATHS: usize = 2000;
/// Entries in the `git status` snapshot kept per root.
const MAX_STATUS: usize = 20_000;
/// Minimum time between two sweeps.
const SWEEP_EVERY_MS: i64 = 2000;

/// Pids writing a file, and the chain of parents of a pid (`[pid, ppid, ...]`).
#[derive(Clone)]
pub struct FdProbe {
    pub writers: Arc<dyn Fn(&Path) -> Vec<u32> + Send + Sync>,
    pub ancestry: Arc<dyn Fn(u32) -> Vec<u32> + Send + Sync>,
}

impl FdProbe {
    /// The platform probe: `/proc` on Linux, nothing elsewhere.
    pub fn system() -> FdProbe {
        FdProbe {
            writers: Arc::new(system_writers),
            ancestry: Arc::new(system_ancestry),
        }
    }
}

#[cfg(target_os = "linux")]
fn system_writers(path: &Path) -> Vec<u32> {
    use std::os::unix::ffi::OsStrExt;
    let mut out = Vec::new();
    let Ok(procs) = std::fs::read_dir("/proc") else {
        return out;
    };
    for (n, e) in procs.flatten().enumerate() {
        if n > 8000 {
            break;
        }
        let name = e.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(fds) = std::fs::read_dir(e.path().join("fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            let Ok(target) = std::fs::read_link(fd.path()) else {
                continue;
            };
            if target.as_os_str().as_bytes() != path.as_os_str().as_bytes() {
                continue;
            }
            let info = e.path().join("fdinfo").join(fd.file_name());
            let flags = std::fs::read_to_string(info)
                .ok()
                .and_then(|t| {
                    t.lines()
                        .find_map(|l| l.strip_prefix("flags:"))
                        .and_then(|f| u32::from_str_radix(f.trim(), 8).ok())
                })
                .unwrap_or(0);
            if flags & 0b11 != 0 {
                out.push(pid);
                break;
            }
        }
    }
    out
}

#[cfg(not(target_os = "linux"))]
fn system_writers(_path: &Path) -> Vec<u32> {
    Vec::new()
}

#[cfg(target_os = "linux")]
fn system_ancestry(pid: u32) -> Vec<u32> {
    let mut out = vec![pid];
    let mut cur = pid;
    for _ in 0..64 {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{cur}/stat")) else {
            break;
        };
        // `pid (comm) state ppid ...`: comm may hold spaces and parentheses.
        let Some(rest) = stat.rsplit_once(')').map(|(_, r)| r) else {
            break;
        };
        let ppid = rest
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0);
        if ppid <= 1 {
            break;
        }
        out.push(ppid);
        cur = ppid;
    }
    out
}

#[cfg(not(target_os = "linux"))]
fn system_ancestry(pid: u32) -> Vec<u32> {
    vec![pid]
}

struct Pending {
    root: String,
    path: String,
    at_ms: i64,
    op: vc::Op,
}

struct Watched {
    /// `None` for a root registered without a platform watcher (tests).
    _watcher: Option<notify::RecommendedWatcher>,
    since_ms: i64,
}

#[derive(Default)]
struct Inner {
    watchers: HashMap<String, Watched>,
    last_sweep_ms: i64,
    probe: Option<FdProbe>,
}

/// Watcher state of the server.
#[derive(Default)]
pub struct Watch {
    inner: Mutex<Inner>,
    /// Watcher events waiting to settle; shared with the platform callbacks.
    pending: Arc<Mutex<Vec<Pending>>>,
}

/// Replace the open-file probe (tests, or a platform with a better one).
pub fn set_probe(server: &Server, probe: FdProbe) {
    server.collision.watch.inner.lock().unwrap().probe = Some(probe);
}

/// Queue a file-system event as the platform watcher would (tests). `rel` is repo relative.
pub fn feed_fs(server: &Server, root: &str, rel: &str, op: vc::Op, at_ms: i64) {
    let mut p = server.collision.watch.pending.lock().unwrap();
    if p.len() < MAX_PENDING {
        p.push(Pending {
            root: root.to_string(),
            path: rel.to_string(),
            at_ms,
            op,
        });
    }
}

/// The worker thread: one tick every quarter second for the life of the server.
pub(super) fn spawn(server: &Arc<Server>) {
    let weak = Arc::downgrade(server);
    let _ = std::thread::Builder::new()
        .name("vk-collision".into())
        .spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(250));
                let Some(server) = weak.upgrade() else {
                    return;
                };
                tick(&server, now_ms());
            }
        });
}

/// One step of the worker: sync watchers, settle and attribute watcher events, poll `git status`
/// where due, sweep.
pub fn tick(server: &Arc<Server>, now: i64) {
    super::ensure_loaded(server);
    let cfg = config(server);
    if !cfg.enabled {
        sweep_if_due(server, now);
        return;
    }
    let active = cfg.fs_attribution != vk_config::FsAttribution::Off;
    let shared = shared_roots(server);
    // A checkout nobody shares any more and with nothing left to remember is dropped (its
    // `status` baseline goes with it).
    server
        .collision
        .inner
        .lock()
        .unwrap()
        .roots
        .retain(|root, rs| shared.contains_key(root) || !rs.tracker.is_empty());
    if active {
        sync_watchers(server, &cfg, &shared, now);
        flush(server, &cfg, now);
        for (root, runs) in &shared {
            poll_due(server, &cfg, root, runs, now);
        }
    } else {
        server
            .collision
            .watch
            .inner
            .lock()
            .unwrap()
            .watchers
            .clear();
        server.collision.watch.pending.lock().unwrap().clear();
    }
    sweep_if_due(server, now);
}

fn sweep_if_due(server: &Server, now: i64) {
    {
        let mut w = server.collision.watch.inner.lock().unwrap();
        if now - w.last_sweep_ms < SWEEP_EVERY_MS {
            return;
        }
        w.last_sweep_ms = now;
    }
    sweep(server, now);
}

/// Checkouts worth watching: two or more live runs, or a claim of a live run.
pub(super) fn shared_roots(server: &Server) -> BTreeMap<String, Vec<AgentRun>> {
    let mut by_root: BTreeMap<String, Vec<AgentRun>> = BTreeMap::new();
    for r in live_runs(server) {
        if let Some(root) = root_of(server, &r) {
            by_root.entry(root).or_default().push(r);
        }
    }
    let claimed: BTreeSet<String> = {
        let g = server.collision.inner.lock().unwrap();
        g.claims
            .iter()
            .filter(|c| {
                by_root
                    .get(&c.root)
                    .is_some_and(|rs| rs.iter().any(|r| r.id == c.run))
            })
            .map(|c| c.root.clone())
            .collect()
    };
    by_root.retain(|root, runs| runs.len() >= 2 || claimed.contains(root));
    by_root
}

fn sync_watchers(
    server: &Server,
    cfg: &vk_config::Collision,
    shared: &BTreeMap<String, Vec<AgentRun>>,
    now: i64,
) {
    let mut w = server.collision.watch.inner.lock().unwrap();
    w.watchers.retain(|root, _| shared.contains_key(root));
    if cfg.watcher != "os" {
        return;
    }
    for root in shared.keys() {
        if w.watchers.contains_key(root) || w.watchers.len() >= MAX_WATCHED {
            continue;
        }
        let watcher = start_os_watcher(
            root,
            cfg.ignore.clone(),
            server.collision.watch.pending.clone(),
        );
        w.watchers.insert(
            root.clone(),
            Watched {
                _watcher: watcher,
                since_ms: now,
            },
        );
    }
}

fn start_os_watcher(
    root: &str,
    ignore: Vec<String>,
    pending: Arc<Mutex<Vec<Pending>>>,
) -> Option<notify::RecommendedWatcher> {
    use notify::{EventKind, RecursiveMode, Watcher};
    let root_owned = root.to_string();
    let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(ev) = res else { return };
        let op = match ev.kind {
            EventKind::Create(_) => vc::Op::Create,
            EventKind::Remove(_) => vc::Op::Delete,
            EventKind::Modify(_) => vc::Op::Modify,
            _ => return,
        };
        let at = now_ms();
        let mut q = pending.lock().unwrap();
        for p in ev.paths {
            let Some(rel) = vc::relativize(Path::new(&root_owned), &p.to_string_lossy()) else {
                continue;
            };
            if vc::is_ignored_path(&rel, &ignore) {
                continue;
            }
            if q.len() < MAX_PENDING {
                q.push(Pending {
                    root: root_owned.clone(),
                    path: rel,
                    at_ms: at,
                    op,
                });
            }
        }
    })
    .ok()?;
    w.watch(Path::new(root), RecursiveMode::Recursive).ok()?;
    Some(w)
}

// ---- attribution ----------------------------------------------------------------------------

/// What the tracker knows about the live runs of `root` at `now`.
fn run_views(server: &Server, root: &str, runs: &[AgentRun], now: i64) -> Vec<vc::RunView> {
    let g = server.collision.inner.lock().unwrap();
    runs.iter()
        .filter(|r| run_alive(r))
        .map(|r| vc::RunView {
            run: r.id.clone(),
            working: r.execution.value == Execution::Working,
            in_flight: g
                .in_flight
                .iter()
                .filter(|f: &&InFlight| {
                    f.run == r.id
                        && f.root == root
                        && f.ended_ms.is_none_or(|e| now - e <= IN_FLIGHT_GRACE_MS)
                })
                .map(|f| f.path.clone())
                .collect(),
        })
        .collect()
}

/// Runs whose process held `rel` open for writing (`aggressive` only).
fn fd_writers(server: &Server, root: &str, rel: &str, runs: &[AgentRun]) -> Vec<String> {
    let probe = {
        let mut w = server.collision.watch.inner.lock().unwrap();
        w.probe.get_or_insert_with(FdProbe::system).clone()
    };
    let abs = Path::new(root).join(rel);
    let pids = (probe.writers)(&abs);
    if pids.is_empty() {
        return vec![];
    }
    let owners: Vec<(String, u32)> = server.with_core(|c| {
        runs.iter()
            .filter_map(|r| {
                c.pane(&r.pane)
                    .and_then(|p| p.child_pid)
                    .map(|pid| (r.id.clone(), pid))
            })
            .collect()
    });
    let mut out = BTreeSet::new();
    for pid in pids {
        let chain = (probe.ancestry)(pid);
        for (run, root_pid) in &owners {
            if chain.contains(root_pid) {
                out.insert(run.clone());
            }
        }
    }
    out.into_iter().collect()
}

/// Whether an adapter reported a write of `path` since `since_ms`: that report explains the
/// file-system event (two writers of one file inside that short window are not told apart).
fn explained(server: &Server, root: &str, path: &str, since_ms: i64) -> bool {
    let g = server.collision.inner.lock().unwrap();
    g.roots.get(root).is_some_and(|rs| {
        rs.tracker.touches().any(|t| {
            t.path == path
                && t.kind == vc::Kind::Write
                && t.is_certain()
                && t.source == vc::Source::Adapter
                && t.at_ms >= since_ms
        })
    })
}

/// Attribute a changed path and record the touch. `explain_ms`: how far back an adapter report
/// counts as the cause.
#[allow(clippy::too_many_arguments)]
fn attribute_and_record(
    server: &Server,
    cfg: &vk_config::Collision,
    root: &str,
    runs: &[AgentRun],
    rel: &str,
    op: vc::Op,
    at: i64,
    source: vc::Source,
    explain_ms: i64,
) {
    if explained(server, root, rel, at - explain_ms) {
        return;
    }
    let views = run_views(server, root, runs, at);
    if views.is_empty() {
        return;
    }
    let writers = if cfg.fs_attribution == vk_config::FsAttribution::Aggressive {
        fd_writers(server, root, rel, runs)
    } else {
        vec![]
    };
    match vc::attribute(rel, &views, &writers) {
        vc::Attribution::Run(r) => record(
            server,
            cfg,
            root,
            vc::Touch::write(&r, rel, at, source, op.as_str()),
        ),
        vc::Attribution::Ambiguous(c) => {
            let mut t = vc::Touch::ambiguous(c, rel, at, source);
            t.op = op.as_str().into();
            record(server, cfg, root, t);
        }
        vc::Attribution::None => {}
    }
}

/// Settle and attribute queued watcher events older than `settle`.
pub(super) fn flush(server: &Server, cfg: &vk_config::Collision, now: i64) {
    let settle = cfg.settle.as_millis() as i64;
    let ready: Vec<Pending> = {
        let mut q = server.collision.watch.pending.lock().unwrap();
        let (ready, wait): (Vec<Pending>, Vec<Pending>) = std::mem::take(&mut *q)
            .into_iter()
            .partition(|p| now - p.at_ms >= settle);
        *q = wait;
        ready
    };
    if ready.is_empty() {
        return;
    }
    let mut by_root: BTreeMap<String, BTreeMap<String, (vc::Op, i64)>> = BTreeMap::new();
    for p in ready {
        let e = by_root
            .entry(p.root)
            .or_default()
            .entry(p.path)
            .or_insert((p.op, p.at_ms));
        e.1 = e.1.max(p.at_ms);
    }
    let shared = shared_roots(server);
    for (root, paths) in by_root {
        let Some(runs) = shared.get(&root) else {
            continue;
        };
        let names: Vec<String> = paths.keys().take(MAX_FLUSH_PATHS).cloned().collect();
        let ignored = crate::review::interval::ignored_paths(Path::new(&root), &names);
        for (path, (op, at)) in paths.into_iter().take(MAX_FLUSH_PATHS) {
            if ignored.contains(&path) {
                continue;
            }
            attribute_and_record(
                server,
                cfg,
                &root,
                runs,
                &path,
                op,
                at,
                vc::Source::Watcher,
                IN_FLIGHT_GRACE_MS + cfg.settle.as_millis() as i64,
            );
        }
    }
}

// ---- git status poll ------------------------------------------------------------------------

/// `git status --porcelain=v1 -z` of `root` as `path -> (fingerprint, op)`, `None` when git
/// fails (not a repository, a contained checkout whose `.git` was swapped).
pub fn git_snapshot(root: &Path, ignore: &[String]) -> Option<BTreeMap<String, (String, vc::Op)>> {
    let safety = vk_tasks::safety_args(root).ok()?;
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(&safety)
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=normal"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let mut map = BTreeMap::new();
    for e in vc::parse_porcelain_z(&out.stdout)
        .into_iter()
        .take(MAX_STATUS)
    {
        if vc::is_ignored_path(&e.path, ignore) {
            continue;
        }
        let meta = std::fs::metadata(root.join(&e.path)).ok();
        let size = meta.as_ref().map(|m| m.len());
        let mtime = meta
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64);
        let op = vc::status_op(&e.xy);
        map.insert(e.path.clone(), (vc::fingerprint(&e.xy, size, mtime), op));
    }
    Some(map)
}

fn poll_due(server: &Server, cfg: &vk_config::Collision, root: &str, runs: &[AgentRun], now: i64) {
    // Only while an agent in the repo is working.
    if !runs.iter().any(|r| r.execution.value == Execution::Working) {
        return;
    }
    let due = {
        let mut g = server.collision.inner.lock().unwrap();
        let rs = g.roots.entry(root.to_string()).or_default();
        if now - rs.last_poll_ms < cfg.poll_interval.as_millis() as i64 {
            false
        } else {
            rs.last_poll_ms = now;
            true
        }
    };
    if due {
        poll_now(server, cfg, root, runs, now);
    }
}

/// Take a `git status` snapshot of `root` and attribute what changed since the last one (the
/// first snapshot is the baseline and reports nothing).
pub fn poll_now(
    server: &Server,
    cfg: &vk_config::Collision,
    root: &str,
    runs: &[AgentRun],
    now: i64,
) {
    let Some(snap) = git_snapshot(Path::new(root), &cfg.ignore) else {
        return;
    };
    let prev = {
        let mut g = server.collision.inner.lock().unwrap();
        let rs = g.roots.entry(root.to_string()).or_default();
        let prev = rs.git_prev.take();
        rs.git_prev = Some(
            snap.iter()
                .map(|(p, (fp, _))| (p.clone(), fp.clone()))
                .collect(),
        );
        prev
    };
    let changes = vc::status_changes(prev.as_ref(), &snap);
    if changes.is_empty() {
        return;
    }
    // Paths git ignores never show in `status`; a path an adapter reported in the last poll
    // period explains its own change.
    let explain = cfg.poll_interval.as_millis() as i64 + 2000;
    for (path, op) in changes {
        attribute_and_record(
            server,
            cfg,
            root,
            runs,
            &path,
            op,
            now,
            vc::Source::Git,
            explain,
        );
    }
}

// ---- status ---------------------------------------------------------------------------------

/// The checkouts the tracker follows (for `collision.status`).
pub(super) fn roots_status(server: &Server) -> Vec<Value> {
    let shared = shared_roots(server);
    let g = server.collision.inner.lock().unwrap();
    let w = server.collision.watch.inner.lock().unwrap();
    shared
        .iter()
        .map(|(root, runs)| {
            let rs = g.roots.get(root);
            json!({
                "root": root,
                "runs": runs.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
                "working": runs.iter().filter(|r| r.execution.value == Execution::Working).count(),
                "watching": w.watchers.contains_key(root),
                "watching_since_ms": w.watchers.get(root).map(|x| x.since_ms),
                "touches": rs.map(|r| r.tracker.len()).unwrap_or(0),
                "last_poll_ms": rs.map(|r| r.last_poll_ms).filter(|t| *t > 0),
            })
        })
        .collect()
}
