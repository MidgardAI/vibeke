//! Shared Codex app-server (04 §6.2, `agents.harness.codex.headless_shared = true`): one
//! `codex app-server` per Vibeke session, multiplexing the threads of every headless Codex run.
//!
//! Each run keeps its own pane, pipe-mode holder and adapter; its child is a **relay**
//! (`vibeke codex-mux --socket <s> -- codex app-server …`) that bridges its stdio to the
//! session's mux socket, starting the mux (`--serve`, detached, one per socket under a lock
//! file) when none answers. So the per-run adapter, its journal, replay and reconcile are
//! unchanged: each run sees a private app-server that happens to be shared.
//!
//! The mux ([`Mux`]):
//! - `initialize` reaches the app-server once; later clients get the cached result, and only
//!   the first `initialized` notification is forwarded.
//! - Client request ids are rewritten to mux-unique ids and restored on the response.
//! - A thread belongs to the client whose `thread/start|resume|fork` response carried it.
//!   Server requests and notifications that name a thread (`threadId`, `thread.id`,
//!   `conversationId`) go to its owner; notifications for a thread whose owner is not known
//!   yet are held (bounded) until it is. `serverRequest/resolved` follows its request.
//!   Everything else (e.g. `account/rateLimits/updated`) is broadcast.
//! - A server request for a thread with no live owner is refused (`-32603`).
//! - The mux exits (killing the app-server) 30 s after its last client left, and when the
//!   app-server exits (every relay then sees EOF and its run ends).
//!
//! Isolated runs never share: their app-server must run inside the run's box.

use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Notifications held per not-yet-owned thread.
const HELD_MAX: usize = 256;
/// The mux exits this long after its last client disconnected.
const IDLE_EXIT: Duration = Duration::from_secs(30);

/// Where a mux output goes.
#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    Server(Value),
    Client(u64, Value),
}

enum Init {
    NotSent,
    /// Sent; clients waiting for the result: (client, their request id).
    Pending(Vec<(u64, Value)>),
    Done(Value),
}

/// Routing core of the mux (no I/O).
pub struct Mux {
    next: u64,
    /// Mux request id → (client, original id, method).
    requests: HashMap<u64, (u64, Value, String)>,
    threads: HashMap<String, u64>,
    /// Server request id → client.
    server_requests: HashMap<String, u64>,
    held: HashMap<String, Vec<Value>>,
    init: Init,
    initialized_sent: bool,
    clients: HashSet<u64>,
}

impl Default for Mux {
    fn default() -> Self {
        Mux {
            next: 1,
            requests: HashMap::new(),
            threads: HashMap::new(),
            server_requests: HashMap::new(),
            held: HashMap::new(),
            init: Init::NotSent,
            initialized_sent: false,
            clients: HashSet::new(),
        }
    }
}

fn id_key(id: &Value) -> String {
    id.as_str()
        .map(str::to_string)
        .unwrap_or_else(|| id.to_string())
}

/// The thread a message is about.
fn thread_of(v: &Value) -> Option<String> {
    let p = v.get("params")?;
    p.get("threadId")
        .or_else(|| p.pointer("/thread/id"))
        .or_else(|| p.get("conversationId"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

impl Mux {
    pub fn connect(&mut self, c: u64) {
        self.clients.insert(c);
    }

    /// A client left: its threads and pending requests are forgotten.
    pub fn disconnect(&mut self, c: u64) {
        self.clients.remove(&c);
        self.threads.retain(|_, o| *o != c);
        self.server_requests.retain(|_, o| *o != c);
        if let Init::Pending(w) = &mut self.init {
            w.retain(|(x, _)| *x != c);
        }
    }

    pub fn clients(&self) -> usize {
        self.clients.len()
    }

    pub fn from_client(&mut self, c: u64, mut v: Value) -> Vec<Out> {
        let method = v.get("method").and_then(Value::as_str).map(str::to_string);
        let id = v.get("id").cloned();
        match (method, id) {
            (Some(m), Some(id)) => {
                if m == "initialize" {
                    match &mut self.init {
                        Init::Done(r) => {
                            return vec![Out::Client(c, json!({"id": id, "result": r.clone()}))];
                        }
                        Init::Pending(w) => {
                            w.push((c, id));
                            return vec![];
                        }
                        Init::NotSent => self.init = Init::Pending(vec![(c, id.clone())]),
                    }
                }
                let mid = self.next;
                self.next += 1;
                self.requests.insert(mid, (c, id, m));
                v["id"] = json!(mid);
                vec![Out::Server(v)]
            }
            (Some(m), None) => {
                if m == "initialized" {
                    if self.initialized_sent {
                        return vec![];
                    }
                    self.initialized_sent = true;
                }
                vec![Out::Server(v)]
            }
            // A response to a server request.
            (None, Some(id)) => {
                self.server_requests.remove(&id_key(&id));
                vec![Out::Server(v)]
            }
            (None, None) => vec![],
        }
    }

    pub fn from_server(&mut self, mut v: Value) -> Vec<Out> {
        let method = v.get("method").and_then(Value::as_str).map(str::to_string);
        let id = v.get("id").cloned();
        match (method, id) {
            (None, Some(id)) => {
                let Some((c, orig, m)) = id.as_u64().and_then(|i| self.requests.remove(&i)) else {
                    return vec![];
                };
                if m == "initialize" {
                    let waiters = match std::mem::replace(&mut self.init, Init::NotSent) {
                        Init::Pending(w) => w,
                        _ => vec![(c, orig.clone())],
                    };
                    if let Some(r) = v.get("result") {
                        self.init = Init::Done(r.clone());
                    }
                    return waiters
                        .into_iter()
                        .filter(|(x, _)| self.clients.contains(x))
                        .map(|(x, i)| {
                            let mut r = v.clone();
                            r["id"] = i;
                            Out::Client(x, r)
                        })
                        .collect();
                }
                let mut out = Vec::new();
                if matches!(m.as_str(), "thread/start" | "thread/resume" | "thread/fork")
                    && let Some(t) = v.pointer("/result/thread/id").and_then(Value::as_str)
                {
                    self.threads.insert(t.to_string(), c);
                    for h in self.held.remove(t).unwrap_or_default() {
                        out.push(Out::Client(c, h));
                    }
                }
                v["id"] = orig;
                if self.clients.contains(&c) {
                    // The response first, then what was held for the thread.
                    out.insert(0, Out::Client(c, v));
                } else {
                    out.clear();
                }
                out
            }
            (Some(_), Some(id)) => {
                let owner = thread_of(&v)
                    .and_then(|t| self.threads.get(&t).copied())
                    .or_else(|| {
                        // No thread named: the only client, if there is exactly one.
                        (thread_of(&v).is_none() && self.clients.len() == 1)
                            .then(|| self.clients.iter().next().copied())
                            .flatten()
                    });
                match owner {
                    Some(c) => {
                        self.server_requests.insert(id_key(&id), c);
                        vec![Out::Client(c, v)]
                    }
                    None => vec![Out::Server(
                        json!({"id": id, "error": {"code": -32603, "message": "vibeke codex-mux: no client owns this thread"}}),
                    )],
                }
            }
            (Some(m), None) => {
                if m == "serverRequest/resolved"
                    && let Some(rid) = v.pointer("/params/requestId")
                    && let Some(c) = self.server_requests.remove(&id_key(rid))
                {
                    return vec![Out::Client(c, v)];
                }
                match thread_of(&v) {
                    Some(t) => match self.threads.get(&t) {
                        Some(c) => vec![Out::Client(*c, v)],
                        None => {
                            let h = self.held.entry(t).or_default();
                            if h.len() < HELD_MAX {
                                h.push(v);
                            }
                            vec![]
                        }
                    },
                    None => self
                        .clients
                        .iter()
                        .map(|c| Out::Client(*c, v.clone()))
                        .collect(),
                }
            }
            (None, None) => vec![],
        }
    }
}

// ---- processes ------------------------------------------------------------------------------

/// The pane child of a shared run: `<vibeke> codex-mux --socket <sock> -- <app-server argv…>`.
pub fn relay_argv(bin: &Path, socket: &Path, argv: Vec<String>) -> Vec<String> {
    let mut v = vec![
        bin.to_string_lossy().into_owned(),
        "codex-mux".into(),
        "--socket".into(),
        socket.to_string_lossy().into_owned(),
        "--".into(),
    ];
    v.extend(argv);
    v
}

/// `vibeke codex-mux --socket <path> [--serve] -- <app-server argv…>`.
pub fn main(args: &[String]) -> i32 {
    let mut socket = None;
    let mut serve = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--socket" => {
                socket = args.get(i + 1).map(PathBuf::from);
                i += 1;
            }
            "--serve" => serve = true,
            "--" => {
                i += 1;
                break;
            }
            _ => break,
        }
        i += 1;
    }
    let argv = args[i.min(args.len())..].to_vec();
    let (Some(socket), false) = (socket, argv.is_empty()) else {
        eprintln!("usage: vibeke codex-mux --socket <path> [--serve] -- <codex app-server argv…>");
        return 2;
    };
    if serve {
        return match serve_main(&socket, &argv) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("codex-mux: {e}");
                1
            }
        };
    }
    match relay(&socket, &argv) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("codex-mux: {e}");
            1
        }
    }
}

/// Bridge stdio to the mux, starting it when none answers.
fn relay(socket: &Path, argv: &[String]) -> std::io::Result<()> {
    let stream = match UnixStream::connect(socket) {
        Ok(s) => s,
        Err(_) => {
            spawn_mux(socket, argv)?;
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                match UnixStream::connect(socket) {
                    Ok(s) => break s,
                    Err(e) if Instant::now() > deadline => return Err(e),
                    Err(_) => std::thread::sleep(Duration::from_millis(50)),
                }
            }
        }
    };
    let mut up = stream.try_clone()?;
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut std::io::stdin().lock(), &mut up);
        let _ = up.shutdown(std::net::Shutdown::Write);
    });
    let mut down = stream;
    std::io::copy(&mut down, &mut std::io::stdout().lock())?;
    Ok(())
}

fn spawn_mux(socket: &Path, argv: &[String]) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe()?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(socket.with_extension("log"))?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("codex-mux")
        .arg("--serve")
        .arg("--socket")
        .arg(socket)
        .arg("--")
        .args(argv)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(log);
    // SAFETY: setsid in the child before exec; async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn()?;
    Ok(())
}

enum Msg {
    Client(u64, Value),
    Closed(u64),
    Accepted(u64, UnixStream),
    Server(Value),
    ServerExited,
}

fn serve_main(socket: &Path, argv: &[String]) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    // One mux per socket: the lock is held for the mux's lifetime.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(socket.with_extension("lock"))?;
    // SAFETY: flock on an fd we own.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Ok(());
    }
    if UnixStream::connect(socket).is_ok() {
        return Ok(());
    }
    let _ = std::fs::remove_file(socket);
    let listener = UnixListener::bind(socket)?;
    let mut child = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()?;
    let mut to_server = child.stdin.take().expect("piped stdin");
    let from_server = child.stdout.take().expect("piped stdout");
    let (tx, rx) = std::sync::mpsc::channel::<Msg>();
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(from_server).lines() {
                let Ok(line) = line else { break };
                if let Ok(v) = serde_json::from_str(&line) {
                    let _ = tx.send(Msg::Server(v));
                }
            }
            let _ = tx.send(Msg::ServerExited);
        });
    }
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut n = 0u64;
            for s in listener.incoming() {
                let Ok(s) = s else { continue };
                n += 1;
                let Ok(r) = s.try_clone() else { continue };
                let _ = tx.send(Msg::Accepted(n, s));
                let tx = tx.clone();
                let c = n;
                std::thread::spawn(move || {
                    for line in BufReader::new(r).lines() {
                        let Ok(line) = line else { break };
                        if let Ok(v) = serde_json::from_str(&line) {
                            let _ = tx.send(Msg::Client(c, v));
                        }
                    }
                    let _ = tx.send(Msg::Closed(c));
                });
            }
        });
    }
    let mut mux = Mux::default();
    let mut writers: HashMap<u64, UnixStream> = HashMap::new();
    let mut idle_since = Some(Instant::now());
    loop {
        let msg = match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(m) => m,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if idle_since.is_some_and(|t| t.elapsed() > IDLE_EXIT) {
                    break;
                }
                continue;
            }
            Err(_) => break,
        };
        let outs = match msg {
            Msg::Accepted(c, s) => {
                mux.connect(c);
                writers.insert(c, s);
                idle_since = None;
                vec![]
            }
            Msg::Closed(c) => {
                mux.disconnect(c);
                writers.remove(&c);
                if mux.clients() == 0 {
                    idle_since = Some(Instant::now());
                }
                vec![]
            }
            Msg::Client(c, v) => mux.from_client(c, v),
            Msg::Server(v) => mux.from_server(v),
            Msg::ServerExited => break,
        };
        for o in outs {
            match o {
                Out::Server(v) => {
                    let mut b = serde_json::to_vec(&v).unwrap_or_default();
                    b.push(b'\n');
                    if to_server
                        .write_all(&b)
                        .and_then(|_| to_server.flush())
                        .is_err()
                    {
                        break;
                    }
                }
                Out::Client(c, v) => {
                    let mut b = serde_json::to_vec(&v).unwrap_or_default();
                    b.push(b'\n');
                    if let Some(w) = writers.get_mut(&c)
                        && w.write_all(&b).is_err()
                    {
                        writers.remove(&c);
                    }
                }
            }
        }
    }
    let _ = std::fs::remove_file(socket);
    let _ = child.kill();
    let _ = child.wait();
    for (_, w) in writers {
        let _ = w.shutdown(std::net::Shutdown::Both);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn to_client(o: &[Out]) -> Vec<(u64, Value)> {
        o.iter()
            .filter_map(|x| match x {
                Out::Client(c, v) => Some((*c, v.clone())),
                _ => None,
            })
            .collect()
    }

    fn to_server(o: &[Out]) -> Vec<Value> {
        o.iter()
            .filter_map(|x| match x {
                Out::Server(v) => Some(v.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn initialize_once_ids_rewritten_threads_routed() {
        let mut m = Mux::default();
        m.connect(1);
        m.connect(2);
        // Both clients initialize; only the first reaches the app-server.
        let o = m.from_client(1, json!({"id": 1, "method": "initialize", "params": {}}));
        let init = to_server(&o)[0].clone();
        assert!(
            m.from_client(2, json!({"id": 1, "method": "initialize"}))
                .is_empty()
        );
        let o = m.from_server(json!({"id": init["id"], "result": {"userAgent": "codex"}}));
        assert_eq!(
            to_client(&o),
            [
                (1, json!({"id": 1, "result": {"userAgent": "codex"}})),
                (2, json!({"id": 1, "result": {"userAgent": "codex"}}))
            ]
        );
        assert_eq!(m.from_client(1, json!({"method": "initialized"})).len(), 1);
        assert!(
            m.from_client(2, json!({"method": "initialized"}))
                .is_empty()
        );
        // A third client gets the cached result at once.
        m.connect(3);
        let o = m.from_client(3, json!({"id": 7, "method": "initialize"}));
        assert_eq!(
            to_client(&o)[0],
            (3, json!({"id": 7, "result": {"userAgent": "codex"}}))
        );

        // Same client ids, distinct mux ids.
        let a = to_server(&m.from_client(
            1,
            json!({"id": 2, "method": "thread/start", "params": {"cwd": "/a"}}),
        ))[0]
            .clone();
        let b = to_server(&m.from_client(
            2,
            json!({"id": 2, "method": "thread/start", "params": {"cwd": "/b"}}),
        ))[0]
            .clone();
        assert_ne!(a["id"], b["id"]);
        // A notification for a thread whose start response has not come yet is held.
        assert!(
            m.from_server(json!({"method": "thread/started", "params": {"thread": {"id": "tb"}}}))
                .is_empty()
        );
        let o = m.from_server(json!({"id": b["id"], "result": {"thread": {"id": "tb"}}}));
        assert_eq!(
            to_client(&o),
            [
                (2, json!({"id": 2, "result": {"thread": {"id": "tb"}}})),
                (
                    2,
                    json!({"method": "thread/started", "params": {"thread": {"id": "tb"}}})
                )
            ]
        );
        m.from_server(json!({"id": a["id"], "result": {"thread": {"id": "ta"}}}));

        // Notifications and server requests follow the thread.
        let o = m.from_server(
            json!({"method": "turn/started", "params": {"threadId": "ta", "turn": {"id": "u1"}}}),
        );
        assert_eq!(to_client(&o)[0].0, 1);
        let o = m.from_server(json!({"id": 50, "method": "item/commandExecution/requestApproval", "params": {"threadId": "tb", "command": "ls"}}));
        assert_eq!(to_client(&o)[0].0, 2);
        let o = m.from_client(2, json!({"id": 50, "result": {"decision": "accept"}}));
        assert_eq!(
            to_server(&o)[0]["id"],
            50,
            "server request ids pass through"
        );
        // Thread-less notifications are broadcast.
        let o = m.from_server(
            json!({"method": "account/rateLimits/updated", "params": {"rateLimits": {}}}),
        );
        let mut cs: Vec<u64> = to_client(&o).iter().map(|x| x.0).collect();
        cs.sort();
        assert_eq!(cs, [1, 2, 3]);
        // serverRequest/resolved follows its request.
        m.from_server(json!({"id": 51, "method": "item/fileChange/requestApproval", "params": {"threadId": "ta"}}));
        let o =
            m.from_server(json!({"method": "serverRequest/resolved", "params": {"requestId": 51}}));
        assert_eq!(to_client(&o)[0].0, 1);
    }

    #[test]
    fn disconnected_clients_lose_their_threads() {
        let mut m = Mux::default();
        m.connect(1);
        m.connect(2);
        let s = to_server(&m.from_client(1, json!({"id": 1, "method": "thread/start"})))[0].clone();
        m.from_server(json!({"id": s["id"], "result": {"thread": {"id": "t1"}}}));
        m.disconnect(1);
        assert_eq!(m.clients(), 1);
        // A request for the orphaned thread is refused, not misrouted.
        let o = m.from_server(json!({"id": 9, "method": "item/commandExecution/requestApproval", "params": {"threadId": "t1"}}));
        assert_eq!(to_server(&o)[0]["error"]["code"], -32603);
        // A response for a request of the gone client is dropped.
        let r = to_server(&m.from_client(2, json!({"id": 3, "method": "thread/list"})))[0].clone();
        m.disconnect(2);
        assert!(
            m.from_server(json!({"id": r["id"], "result": {}}))
                .is_empty()
        );
        // Another client resuming the thread takes it over.
        m.connect(3);
        let s = to_server(&m.from_client(
            3,
            json!({"id": 1, "method": "thread/resume", "params": {"threadId": "t1"}}),
        ))[0]
            .clone();
        m.from_server(json!({"id": s["id"], "result": {"thread": {"id": "t1"}}}));
        let o = m.from_server(json!({"method": "turn/completed", "params": {"threadId": "t1"}}));
        assert_eq!(to_client(&o)[0].0, 3);
    }

    /// The relay and mux processes against a fake app-server: two relays share one process.
    #[test]
    fn relays_share_one_app_server() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("fake-app-server");
        std::fs::write(
            &fake,
            "#!/bin/sh\necho $$ >> \"$0.pids\"\nwhile IFS= read -r l; do\n  id=$(printf '%s' \"$l\" | sed -n 's/.*\"id\":\\([0-9]*\\).*/\\1/p')\n  [ -n \"$id\" ] && printf '{\"id\":%s,\"result\":{\"pid\":%s}}\\n' \"$id\" $$\ndone\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let sock = dir.path().join("mux.sock");
        let argv = vec![fake.to_string_lossy().into_owned()];
        let sock2 = sock.clone();
        let argv2 = argv.clone();
        std::thread::spawn(move || serve_main(&sock2, &argv2));
        let deadline = Instant::now() + Duration::from_secs(5);
        let conn = || loop {
            match UnixStream::connect(&sock) {
                Ok(s) => break s,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                Err(e) => panic!("{e}"),
            }
        };
        let ask = |s: &mut UnixStream, id: u64| -> Value {
            writeln!(s, "{}", json!({"id": id, "method": "thread/list"})).unwrap();
            let mut l = String::new();
            BufReader::new(s.try_clone().unwrap())
                .read_line(&mut l)
                .unwrap();
            serde_json::from_str(&l).unwrap()
        };
        let mut a = conn();
        let mut b = conn();
        let ra = ask(&mut a, 1);
        let rb = ask(&mut b, 1);
        assert_eq!(ra["id"], 1);
        assert_eq!(rb["id"], 1, "each client sees its own ids");
        assert_eq!(ra["result"]["pid"], rb["result"]["pid"], "one app-server");
        let pids = std::fs::read_to_string(dir.path().join("fake-app-server.pids")).unwrap();
        assert_eq!(pids.lines().count(), 1);
    }
}
