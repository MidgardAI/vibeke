//! ACP `terminal/*` for headless ACP runs (04 §6.6): a terminal the agent creates is a **real
//! Vibeke pane**, split below the run's pane, so the user sees the command run.
//!
//! | ACP | Vibeke |
//! |---|---|
//! | `terminal/create {command, args, env, cwd, outputByteLimit}` | a pane running the command (`/bin/sh -c` when no `args`) in `cwd`; `{terminalId}` |
//! | `terminal/output` | the pane's text (scrollback + screen), the newest `outputByteLimit` bytes, `truncated`, `exitStatus` once exited |
//! | `terminal/wait_for_exit` | answered when the pane's process exits: `{exitCode, signal}` |
//! | `terminal/kill` | the pane is closed (its output stays readable) |
//! | `terminal/release` | killed if running; the terminal id is forgotten |
//!
//! **Sandboxing, exactly as ACP `fs/*`.** The working directory must resolve (every component,
//! symlinks included) inside the session cwd, else the request is refused (`-32002`). A run
//! Vibeke isolates gets neither: the server would start the command on the host, outside the
//! run's box, so `clientCapabilities.terminal` (and `fs`) is false and requests are refused.
//!
//! A watcher task per terminal keeps the latest output snapshot (the pane closes when its
//! process exits, so the snapshot is what `terminal/output` reads afterwards) and tells the
//! run's pane when the process exited. Terminal ids and their panes are kept in the run's
//! record: a restarted server watches the panes again (a pane gone meanwhile reports an exit
//! with unknown status), and the journal replay re-issues the requests still unanswered.
//!
//! **At most once.** `terminal/create` first persists an *intent* (the terminal id and the
//! request's native ref, no pane yet) and only then starts the command; a persistence error
//! refuses the request without starting anything. The pane is recorded right after. A replayed
//! or reconciled create that finds an intent without a pane (the server stopped in between)
//! answers that the terminal's state is unknown and never starts the command again.

use super::*;

/// Bytes of output kept per terminal (the newest).
const MAX_OUTPUT: usize = 1 << 20;
/// Lines of scrollback read per snapshot.
const SNAPSHOT_LINES: usize = 5000;

/// A terminal in the run's [`Record`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Saved {
    pub id: String,
    /// Native ref of the `terminal/create` request (a replayed or reconciled create answers
    /// with this terminal instead of starting another).
    pub create_ref: String,
    pub pane: String,
    #[serde(default)]
    pub limit: Option<u64>,
    /// An intent recorded before the command started; `pane` is not known yet. Found on
    /// replay it means the command may or may not have started: never started again.
    #[serde(default)]
    pub pending: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Exit {
    pub code: Option<i64>,
    pub signal: Option<String>,
}

impl Exit {
    fn json(&self) -> Value {
        json!({"exitCode": self.code, "signal": self.signal})
    }
}

#[derive(Debug, Default)]
pub struct Term {
    pub pane: String,
    pub output: String,
    pub exit: Option<Exit>,
    pub limit: Option<u64>,
}

pub type Terms = Arc<Mutex<HashMap<String, Term>>>;

/// A `terminal/*` request (live only).
#[derive(Debug, Clone)]
pub struct Req {
    pub id: Value,
    pub method: String,
    pub params: Value,
}

/// A retained runtime keeps a fast command's final output readable after its pane closes.
pub struct Spawned {
    pub pane: String,
    pub rt: Option<Arc<crate::pane::PaneRt>>,
}

/// Starts a terminal's command: `(cwd, argv, title)` → its pane and retained runtime.
pub type Spawn<'a> =
    dyn FnMut(&std::path::Path, Vec<String>, String) -> Result<Spawned, String> + 'a;

/// What a request needs from the session.
pub struct Ctx<'a> {
    pub server: &'a Arc<Server>,
    /// The run's pane (the terminal panes split from it).
    pub owner: &'a str,
    pub cwd: &'a str,
    pub isolated: bool,
    pub saved: &'a mut Vec<Saved>,
    pub terms: &'a Terms,
    /// `wait_for_exit` requests not answered yet: (terminal id, request id).
    pub waits: &'a mut Vec<(String, Value)>,
    /// Persist the run's record with `saved` as its terminals (must succeed before a command
    /// starts).
    pub persist: &'a mut dyn FnMut(&[Saved]) -> Result<(), String>,
    /// Start the command (a pane split below the run's pane).
    pub spawn: &'a mut Spawn<'a>,
    /// Tests: stop right after the command started, before its pane is recorded (a crash).
    #[cfg(test)]
    pub crash_after_spawn: bool,
}

fn error(id: &Value, code: i64, msg: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": msg}})
}

fn ok(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn signal_name(n: i64) -> String {
    match n {
        1 => "SIGHUP".into(),
        2 => "SIGINT".into(),
        3 => "SIGQUIT".into(),
        6 => "SIGABRT".into(),
        9 => "SIGKILL".into(),
        13 => "SIGPIPE".into(),
        15 => "SIGTERM".into(),
        n => format!("SIG{n}"),
    }
}

/// The directory `path` (relative to `root`) when it resolves inside `root`.
pub fn dir_within(root: &std::path::Path, path: &str) -> Option<std::path::PathBuf> {
    let p = std::path::Path::new(path);
    let p = if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    };
    let root = root.canonicalize().ok()?;
    let full = p.canonicalize().ok()?;
    (full.starts_with(&root) && full.is_dir()).then_some(full)
}

/// The newest `limit` bytes of `s` (cut at a character boundary) and whether it was cut.
pub fn tail(s: &str, limit: Option<u64>) -> (String, bool) {
    let max = limit
        .map(|l| l as usize)
        .unwrap_or(usize::MAX)
        .min(MAX_OUTPUT);
    if s.len() <= max {
        return (s.to_string(), false);
    }
    let mut i = s.len() - max;
    while !s.is_char_boundary(i) {
        i += 1;
    }
    (s[i..].to_string(), true)
}

/// Handle one request: the response to write now, or `None` (a `wait_for_exit` that waits).
pub fn handle(cx: &mut Ctx, native_ref: &str, r: &Req) -> Option<Value> {
    if cx.isolated {
        return Some(error(
            &r.id,
            -32002,
            "terminals are refused: the run is isolated and a terminal would run on the host",
        ));
    }
    let tid = r
        .params
        .get("terminalId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    match r.method.as_str() {
        "terminal/create" => create(cx, native_ref, r),
        "terminal/output" => {
            let pane = cx.terms.lock().unwrap().get(&tid).map(|t| t.pane.clone());
            let Some(pane) = pane else {
                return Some(error(&r.id, -32002, "unknown terminal"));
            };
            if let Some(rt) = cx.server.pane_rt(&pane) {
                snapshot(&rt, cx.terms, &tid);
            }
            let t = cx.terms.lock().unwrap();
            let t = t.get(&tid)?;
            let (output, truncated) = tail(&t.output, t.limit);
            Some(ok(
                &r.id,
                json!({"output": output, "truncated": truncated, "exitStatus": t.exit.as_ref().map(Exit::json)}),
            ))
        }
        "terminal/wait_for_exit" => {
            let exit = match cx.terms.lock().unwrap().get(&tid) {
                None => return Some(error(&r.id, -32002, "unknown terminal")),
                Some(t) => t.exit.clone(),
            };
            match exit {
                Some(e) => Some(ok(&r.id, e.json())),
                None => {
                    if !cx.waits.iter().any(|(_, i)| *i == r.id) {
                        cx.waits.push((tid, r.id.clone()));
                    }
                    None
                }
            }
        }
        "terminal/kill" | "terminal/release" => {
            let t = cx
                .terms
                .lock()
                .unwrap()
                .get(&tid)
                .map(|t| (t.pane.clone(), t.exit.is_some()));
            let Some((pane, exited)) = t else {
                return Some(error(&r.id, -32002, "unknown terminal"));
            };
            if !exited {
                cx.server.close_pane(&pane);
            }
            if r.method == "terminal/release" {
                cx.terms.lock().unwrap().remove(&tid);
                cx.saved.retain(|s| s.id != tid);
            }
            Some(ok(&r.id, Value::Null))
        }
        _ => Some(error(&r.id, -32601, "method not supported by vibeke")),
    }
}

fn create(cx: &mut Ctx, native_ref: &str, r: &Req) -> Option<Value> {
    if let Some(s) = cx.saved.iter().find(|s| s.create_ref == native_ref) {
        if s.pending {
            // The server stopped between starting the command and recording its pane: it may
            // have run. Never start it again.
            return Some(error(
                &r.id,
                -32603,
                "terminal state unknown: the server stopped while starting this command; it is not started again",
            ));
        }
        // Created before (a reconciled or replayed request): the same terminal.
        return Some(ok(&r.id, json!({"terminalId": s.id})));
    }
    create_new(cx, native_ref, r)
}

fn create_new(cx: &mut Ctx, native_ref: &str, r: &Req) -> Option<Value> {
    let fail = |code: i64, msg: &str| Some(error(&r.id, code, msg));
    let root = std::path::Path::new(cx.cwd);
    let want = r
        .params
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or(cx.cwd);
    let Some(dir) = dir_within(root, want) else {
        return fail(-32002, "terminal refused: cwd is outside the session cwd");
    };
    let Some(command) = r
        .params
        .get("command")
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
    else {
        return fail(-32602, "command required");
    };
    let args: Vec<String> = r
        .params
        .get("args")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let mut argv = vec!["/usr/bin/env".to_string()];
    for e in r
        .params
        .get("env")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        if let (Some(n), Some(v)) = (
            e.get("name").and_then(Value::as_str),
            e.get("value").and_then(Value::as_str),
        ) && !n.is_empty()
            && !n.contains('=')
        {
            argv.push(format!("{n}={v}"));
        }
    }
    if args.is_empty() {
        argv.extend(["/bin/sh".into(), "-c".into(), command.to_string()]);
    } else {
        argv.push(command.to_string());
        argv.extend(args.iter().cloned());
    }
    let label: String = if args.is_empty() {
        command.to_string()
    } else {
        let mut v = vec![command.to_string()];
        v.extend(args.iter().cloned());
        harness::shell_join(&v)
    }
    .chars()
    .take(40)
    .collect();
    let id = format!("term_{}", crate::core::ulid());
    let limit = r.params.get("outputByteLimit").and_then(Value::as_u64);
    // The intent is durable before anything runs; without it, nothing runs.
    cx.saved.push(Saved {
        id: id.clone(),
        create_ref: native_ref.to_string(),
        pane: String::new(),
        limit,
        pending: true,
    });
    if let Err(e) = (cx.persist)(cx.saved) {
        cx.saved.retain(|s| s.id != id);
        return fail(
            -32603,
            &format!("terminal not started: its record could not be saved ({e})"),
        );
    }
    // Subscribed before the pane exists: a command that exits at once is still seen exiting.
    let events = cx.server.events.subscribe();
    let Spawned { pane, rt } = match (cx.spawn)(&dir, argv, format!("acp: {label}")) {
        Ok(p) => p,
        Err(e) => {
            // Nothing started: the intent goes.
            cx.saved.retain(|s| s.id != id);
            let _ = (cx.persist)(cx.saved);
            return fail(-32603, &format!("terminal not started: {e}"));
        }
    };
    #[cfg(test)]
    if cx.crash_after_spawn {
        return None;
    }
    if let Some(s) = cx.saved.iter_mut().find(|s| s.id == id) {
        s.pane = pane.clone();
        s.pending = false;
    }
    if let Err(e) = (cx.persist)(cx.saved) {
        tracing::warn!(terminal = %id, error = %e, "terminal pane not recorded");
    }
    cx.terms.lock().unwrap().insert(
        id.clone(),
        Term {
            pane: pane.clone(),
            limit,
            ..Term::default()
        },
    );
    watch(cx.server, &id, &pane, cx.owner, cx.terms, rt, Some(events));
    Some(ok(&r.id, json!({"terminalId": id})))
}

/// After a restart: watch the saved terminals' panes again; a pane gone meanwhile has exited
/// with unknown status.
pub fn rewatch(server: &Arc<Server>, owner: &str, saved: &[Saved], terms: &Terms) {
    for s in saved {
        // An intent without a pane has nothing to watch (its create answers "unknown").
        if s.pending || terms.lock().unwrap().contains_key(&s.id) {
            continue;
        }
        terms.lock().unwrap().insert(
            s.id.clone(),
            Term {
                pane: s.pane.clone(),
                limit: s.limit,
                ..Term::default()
            },
        );
        if let Some(rt) = server.pane_rt(&s.pane) {
            watch(server, &s.id, &s.pane, owner, terms, Some(rt), None);
        } else if let Some(t) = terms.lock().unwrap().get_mut(&s.id) {
            t.exit = Some(Exit {
                code: None,
                signal: None,
            });
        }
    }
}

/// The answers to the `wait_for_exit` requests of terminal `tid` (it exited).
pub fn exited(terms: &Terms, waits: &mut Vec<(String, Value)>, tid: &str) -> Vec<Value> {
    let exit = terms
        .lock()
        .unwrap()
        .get(tid)
        .and_then(|t| t.exit.clone())
        .unwrap_or(Exit {
            code: None,
            signal: None,
        });
    let mut out = Vec::new();
    waits.retain(|(t, id)| {
        if t == tid {
            out.push(ok(id, exit.json()));
            false
        } else {
            true
        }
    });
    out
}

fn snapshot(rt: &crate::pane::PaneRt, terms: &Terms, tid: &str) {
    let text = {
        let sc = rt.screen.lock().unwrap();
        let e = &sc.engine;
        let h = e.history_len();
        let want = SNAPSHOT_LINES.saturating_sub(e.rows() as usize).min(h);
        let mut rows: Vec<vk_proto::render::Row> =
            (h - want..h).filter_map(|i| e.history_row(i)).collect();
        rows.extend(e.visible_rows());
        while rows.last().is_some_and(|r| r.text().trim().is_empty()) {
            rows.pop();
        }
        let mut out = String::new();
        for r in &rows {
            let t = r.text();
            if r.wrapped {
                out.push_str(&t);
            } else {
                out.push_str(t.trim_end());
                out.push('\n');
            }
        }
        out
    };
    if let Some(t) = terms.lock().unwrap().get_mut(tid) {
        t.output = tail(&text, None).0;
    }
}

/// Follow terminal `tid`'s pane until its process exits, then tell the run's pane.
fn watch(
    server: &Arc<Server>,
    tid: &str,
    pane: &str,
    owner: &str,
    terms: &Terms,
    runtime: Option<Arc<crate::pane::PaneRt>>,
    events: Option<tokio::sync::broadcast::Receiver<Arc<vk_store::Event>>>,
) {
    // In-process unit tests have no runtime: nothing to watch.
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    let (server, tid, pane, owner, terms) = (
        server.clone(),
        tid.to_string(),
        pane.to_string(),
        owner.to_string(),
        terms.clone(),
    );
    let mut events = events.unwrap_or_else(|| server.events.subscribe());
    tokio::spawn(async move {
        let exit = match runtime {
            None => Exit {
                code: None,
                signal: None,
            },
            Some(rt) => {
                let mut rev = rt.rev_tx.subscribe();
                snapshot(&rt, &terms, &tid);
                let exit = loop {
                    tokio::select! {
                        r = rev.changed() => {
                            snapshot(&rt, &terms, &tid);
                            if r.is_err() {
                                break Exit { code: None, signal: None };
                            }
                        }
                        e = events.recv() => match e {
                            Ok(ev) if ev.kind == "pane.exited" && ev.subject["pane"] == pane.as_str() => {
                                break Exit {
                                    code: ev.data["code"].as_i64(),
                                    signal: ev.data["signal"].as_i64().map(signal_name),
                                };
                            }
                            Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                            Err(_) => break Exit { code: None, signal: None },
                        },
                        _ = tokio::time::sleep(Duration::from_secs(1)) => {
                            let gone = server.with_core(|c| c.pane(&pane).is_none_or(|p| p.exited));
                            if gone {
                                let code = server.with_core(|c| c.pane(&pane).and_then(|p| p.exit_code));
                                break Exit { code: code.map(i64::from), signal: None };
                            }
                        }
                    }
                };
                snapshot(&rt, &terms, &tid);
                exit
            }
        };
        if let Some(t) = terms.lock().unwrap().get_mut(&tid) {
            t.exit = Some(exit);
        }
        if let Some(rt) = server.pane_rt(&owner) {
            rt.send(crate::pane::PaneCmd::Headless(Cmd::TerminalExited(tid)));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_tail_cuts_at_char_boundaries() {
        assert_eq!(tail("hello", Some(10)), ("hello".into(), false));
        assert_eq!(tail("hello", Some(3)), ("llo".into(), true));
        // "é" is two bytes: a cut inside it moves forward.
        assert_eq!(tail("aé", Some(1)), ("".into(), true));
        assert_eq!(tail("aéb", Some(3)), ("éb".into(), true));
        assert_eq!(signal_name(9), "SIGKILL");
        assert_eq!(signal_name(31), "SIG31");
    }

    #[test]
    fn cwd_must_resolve_inside_the_session_cwd() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("work");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let outside = d.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        assert!(dir_within(&root, "sub").is_some());
        assert!(dir_within(&root, root.to_str().unwrap()).is_some());
        assert!(dir_within(&root, "..").is_none());
        assert!(
            dir_within(&root, "link").is_none(),
            "symlink out of the cwd"
        );
        assert!(dir_within(&root, outside.to_str().unwrap()).is_none());
        assert!(dir_within(&root, "missing").is_none());
    }
}
