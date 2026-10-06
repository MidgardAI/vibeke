//! `session.list|create|stop|rename` and `server.restart` (07 §2.1, §2.2), plus read-only
//! attach (`vibeke attach --readonly`, 07 §5.3): a client that said `client.hello {readonly:
//! true}` gets a render stream whose input, paste, mouse, browser and mutating command frames
//! are refused.

use crate::Server;
use crate::api::{Ctx, R, b, err, internal, invalid, not_found, req, s};
use crate::paths::{Paths, runtime_root, state_root};
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use vk_proto::rpc::ErrorKind;

pub const METHODS: &[(&str, bool)] = &[
    ("session.list", false),
    ("session.create", true),
    ("session.stop", true),
    ("session.rename", true),
    ("server.restart", true),
];

/// Session names: what a runtime/state directory may be called.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn socket_live(path: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

fn entry(server: &Server, name: &str) -> Value {
    let paths = Paths::new(name);
    let running = socket_live(&paths.socket());
    let pid = running
        .then(|| std::fs::read_to_string(paths.pidfile()).ok())
        .flatten()
        .and_then(|s| s.trim().parse::<u32>().ok());
    let mut v = json!({
        "name": name,
        "running": running,
        "socket": paths.socket(),
        "state": paths.state,
        "current": name == server.opts.session,
    });
    if let Some(pid) = pid {
        v["pid"] = json!(pid);
    }
    v
}

/// Session names found under the runtime root (a socket) or the state root (a state db).
fn session_names() -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let scan = |root: PathBuf, marker: &str, out: &mut BTreeSet<String>| {
        let Ok(rd) = std::fs::read_dir(&root) else {
            return;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if valid_name(&name) && e.path().join(marker).exists() {
                out.insert(name);
            }
        }
    };
    scan(runtime_root(), "vibeke.sock", &mut out);
    scan(state_root(), "state.db", &mut out);
    out
}

fn session_list(server: &Server) -> R {
    let mut names = session_names();
    names.insert(server.opts.session.clone());
    let sessions: Vec<Value> = names.iter().map(|n| entry(server, n)).collect();
    Ok(json!({"sessions": sessions}))
}

fn binary(server: &Server) -> PathBuf {
    if server.opts.bin.is_file() {
        server.opts.bin.clone()
    } else {
        std::env::current_exe().unwrap_or_else(|_| server.opts.bin.clone())
    }
}

async fn wait_for(socket: &Path, up: bool, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    loop {
        if socket_live(socket) == up {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn session_create(server: &Server, p: &Value) -> R {
    let name = req(p, "name")?;
    if !valid_name(name) {
        return Err(invalid(
            "session names are 1-64 characters of [A-Za-z0-9._-], not starting with `.`",
        ));
    }
    let paths = Paths::new(name);
    if socket_live(&paths.socket()) {
        return Err(err(
            ErrorKind::Conflict,
            format!("name_taken: session `{name}` is running"),
        )
        .details(json!({"reason": "name_taken"})));
    }
    paths.ensure().map_err(internal)?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.logs().join("server.log"))
        .map_err(internal)?;
    let mut cmd = std::process::Command::new(binary(server));
    cmd.args(["server", "--session", name])
        .env_remove("VIBEKE_SESSION")
        .env_remove("VIBEKE_SOCKET")
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone().map_err(internal)?)
        .stderr(log);
    // Created from inside a pane: the new server is not that pane (09 §3.2).
    for k in crate::run::PANE_IDENTITY_ENV {
        cmd.env_remove(k);
    }
    use std::os::unix::process::CommandExt;
    // SAFETY: setsid between fork and exec is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(internal)?;
    // Reap it in the background (it daemonizes into its own session and outlives us anyway).
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    if !wait_for(&paths.socket(), true, Duration::from_secs(10)).await {
        return Err(internal(format!(
            "session `{name}` did not start within 10 s (see {})",
            paths.logs().join("server.log").display()
        )));
    }
    Ok(json!({"session": entry(server, name)}))
}

/// One JSON-RPC request to another session's server.
async fn call_other(socket: &Path, method: &str, params: Value) -> Result<Value, String> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let s = tokio::net::UnixStream::connect(socket)
        .await
        .map_err(|e| e.to_string())?;
    let (rd, mut wr) = tokio::io::split(s);
    let mut rd = tokio::io::BufReader::new(rd);
    let hello = json!({"jsonrpc": "2.0", "id": 1, "method": "client.hello", "params": {"client": "vibeke-server", "version": vk_proto::VERSION, "api": vk_proto::API_VERSION, "kind": "cli"}});
    let call = json!({"jsonrpc": "2.0", "id": 2, "method": method, "params": params});
    wr.write_all(format!("{hello}\n{call}\n").as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let mut line = String::new();
    loop {
        line.clear();
        if rd.read_line(&mut line).await.map_err(|e| e.to_string())? == 0 {
            return Err("connection closed".into());
        }
        let v: Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
        if v["id"] == 2 {
            return match v.get("error").filter(|e| !e.is_null()) {
                Some(e) => Err(e["message"].as_str().unwrap_or("error").to_string()),
                None => Ok(v["result"].clone()),
            };
        }
    }
}

async fn session_stop(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let name = req(p, "name")?;
    let kill = b(p, "kill_panes").unwrap_or(false);
    if name == server.opts.session {
        Box::pin(crate::api::dispatch(
            server,
            ctx,
            "server.stop",
            &json!({"kill_panes": kill}),
        ))
        .await?;
        return Ok(
            json!({"session": {"name": name, "running": false, "current": true}, "stopped": true}),
        );
    }
    if !valid_name(name) || !session_names().contains(name) {
        return Err(not_found("session", name));
    }
    let socket = Paths::new(name).socket();
    if !socket_live(&socket) {
        return Ok(json!({"session": entry(server, name), "stopped": false}));
    }
    match tokio::time::timeout(
        Duration::from_secs(10),
        call_other(&socket, "server.stop", json!({"kill_panes": kill})),
    )
    .await
    {
        Ok(Ok(_)) | Ok(Err(_)) => {}
        Err(_) => return Err(err(ErrorKind::Timeout, "server.stop timed out")),
    }
    if !wait_for(&socket, false, Duration::from_secs(10)).await {
        return Err(err(
            ErrorKind::Timeout,
            format!("session `{name}` did not stop within 10 s"),
        ));
    }
    Ok(json!({"session": entry(server, name), "stopped": true}))
}

fn session_rename(server: &Server, p: &Value) -> R {
    let name = req(p, "name")?;
    let new = req(p, "new_name")?;
    if !valid_name(new) {
        return Err(invalid(
            "session names are 1-64 characters of [A-Za-z0-9._-], not starting with `.`",
        ));
    }
    if !valid_name(name) || !session_names().contains(name) {
        return Err(not_found("session", name));
    }
    let (from, to) = (Paths::new(name), Paths::new(new));
    let conflict = |reason: &str, msg: String| {
        Err(err(ErrorKind::Conflict, format!("{reason}: {msg}")).details(json!({"reason": reason})))
    };
    if name == server.opts.session || socket_live(&from.socket()) {
        return conflict(
            "session_running",
            format!("stop session `{name}` first (`vibeke session stop {name}`)"),
        );
    }
    // Holders of a server stopped with kill_panes false are still listening in its runtime
    // dir; moving their sockets under them would orphan the panes.
    if let Ok(rd) = std::fs::read_dir(from.holders())
        && rd.flatten().any(|e| socket_live(&e.path()))
    {
        return conflict(
            "holders_live",
            format!(
                "session `{name}` still has running panes; stop it with --kill-panes or attach and close them"
            ),
        );
    }
    if to.runtime.exists() || to.state.exists() {
        return conflict("name_taken", format!("session `{new}` exists"));
    }
    if from.state.exists() {
        std::fs::rename(&from.state, &to.state).map_err(internal)?;
    }
    if from.runtime.exists() {
        std::fs::rename(&from.runtime, &to.runtime).map_err(internal)?;
    }
    Ok(json!({"session": entry(server, new), "previous_name": name}))
}

/// `server.restart {binary?}`: exec the (new) server binary in place. Holders keep running
/// and the new server reattaches them (01 §1.2); the pid stays the same.
fn server_restart(server: &Arc<Server>, p: &Value) -> R {
    let bin = match s(p, "binary") {
        Some(path) => {
            use std::os::unix::fs::PermissionsExt;
            let pb = PathBuf::from(path);
            let ok = std::fs::metadata(&pb)
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
            if !pb.is_absolute() || !ok {
                return Err(invalid(format!(
                    "binary must be an absolute path to an executable file: {path}"
                )));
            }
            pb
        }
        None => binary(server),
    };
    for rt in server.panes.lock().unwrap().values() {
        rt.send(crate::pane::PaneCmd::Snapshot);
    }
    let srv = server.clone();
    let session = server.opts.session.clone();
    let exe = bin.clone();
    tokio::spawn(async move {
        // Let the response and the pane snapshots go out first.
        tokio::time::sleep(Duration::from_millis(300)).await;
        srv.housekeeping();
        tracing::info!(binary = %exe.display(), "server.restart: exec");
        use std::os::unix::process::CommandExt;
        // Every descriptor of the server is close-on-exec (the listener, the state lock, the
        // db, client connections): the new image binds and locks afresh.
        let e = std::process::Command::new(&exe)
            .args(["server", "--session", &session])
            .exec();
        tracing::error!(error = %e, "server.restart: exec failed; still running the old server");
    });
    Ok(json!({"new_pid": std::process::id(), "binary": bin}))
}

/// Dispatch hook for `session.*` (except `session.snapshot`) and `server.restart`.
pub async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    Some(match method {
        "session.list" => session_list(server),
        "session.create" => session_create(server, p).await,
        "session.stop" => session_stop(server, ctx, p).await,
        "session.rename" => session_rename(server, p),
        "server.restart" => server_restart(server, p),
        _ => return None,
    })
}

// ---- read-only attach -----------------------------------------------------------------------

fn readonly_clients() -> &'static Mutex<HashSet<String>> {
    static R: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    R.get_or_init(Mutex::default)
}

pub fn set_readonly(client: &str, on: bool) {
    let mut r = readonly_clients().lock().unwrap();
    if on {
        r.insert(client.to_string());
    } else {
        r.remove(client);
    }
}

pub fn is_readonly(client: &str) -> bool {
    readonly_clients().lock().unwrap().contains(client)
}

/// Whether `method` changes state (its `METHODS` flag); unknown methods count as mutating.
pub fn is_mutating(method: &str) -> bool {
    crate::api_schema::method_tables()
        .iter()
        .flat_map(|(_, t)| t.iter())
        .find(|(n, _)| *n == method)
        .is_none_or(|(_, m)| *m)
}

/// The refusal for a read-only client's mutating API call.
pub fn readonly_refusal(method: &str) -> vk_proto::rpc::RpcError {
    err(
        ErrorKind::PermissionDenied,
        format!("{method}: this client attached read-only"),
    )
    .details(json!({"reason": "readonly"}))
}

/// Render-stream gate for a read-only client: `Ok(frame)` to handle it (a `ViewHint` never makes
/// the client the geometry leader), `Err(reply)` to drop it, answering with `reply` (input acks
/// `Rejected`, mutating commands `permission_denied`).
pub fn readonly_filter(
    client: &str,
    f: vk_proto::render::ClientFrame,
) -> Result<vk_proto::render::ClientFrame, Option<vk_proto::render::ServerFrame>> {
    use vk_proto::render::{AckStatus, ClientFrame, ServerFrame};
    if !is_readonly(client) {
        return Ok(f);
    }
    let rejected = |input_id: u64| {
        Err(Some(ServerFrame::InputAck {
            input_id,
            status: AckStatus::Rejected,
        }))
    };
    match f {
        ClientFrame::Key { input_id, .. }
        | ClientFrame::RawInput { input_id, .. }
        | ClientFrame::Mouse { input_id, .. }
        | ClientFrame::Paste { input_id, .. }
        | ClientFrame::SyncInput { input_id, .. }
        | ClientFrame::Browser { input_id, .. } => rejected(input_id),
        ClientFrame::Command { req, json } => {
            let method = serde_json::from_str::<Value>(&json)
                .ok()
                .and_then(|v| v["method"].as_str().map(str::to_string))
                .unwrap_or_default();
            if is_mutating(&method) {
                let id = serde_json::from_str::<Value>(&json)
                    .ok()
                    .map(|v| v["id"].clone())
                    .unwrap_or(Value::Null);
                let resp = vk_proto::rpc::Response::err(id, readonly_refusal(&method));
                Err(Some(ServerFrame::CommandResult {
                    req,
                    json: serde_json::to_string(&resp).unwrap_or_default(),
                }))
            } else {
                Ok(ClientFrame::Command { req, json })
            }
        }
        ClientFrame::ViewHint { panes, .. } => Ok(ClientFrame::ViewHint {
            panes,
            active: false,
        }),
        other => Ok(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readonly_gate() {
        use vk_proto::render::{ClientFrame, ServerFrame};
        let f = ClientFrame::RawInput {
            input_id: 7,
            pane: "p".into(),
            bytes: b"x".to_vec(),
        };
        assert!(readonly_filter("rw-client", f.clone()).is_ok());
        set_readonly("ro-client", true);
        assert!(matches!(
            readonly_filter("ro-client", f),
            Err(Some(ServerFrame::InputAck { input_id: 7, .. }))
        ));
        let read = ClientFrame::Command {
            req: 1,
            json: r#"{"jsonrpc":"2.0","id":1,"method":"pane.list","params":{}}"#.into(),
        };
        assert!(readonly_filter("ro-client", read).is_ok());
        let write = ClientFrame::Command {
            req: 2,
            json: r#"{"jsonrpc":"2.0","id":2,"method":"pane.close","params":{}}"#.into(),
        };
        match readonly_filter("ro-client", write) {
            Err(Some(ServerFrame::CommandResult { json, .. })) => {
                assert!(json.contains("read-only"), "{json}")
            }
            other => panic!("{other:?}"),
        }
        set_readonly("ro-client", false);
    }

    #[test]
    fn names() {
        assert!(valid_name("default"));
        assert!(valid_name("work-2.b_c"));
        assert!(!valid_name(""));
        assert!(!valid_name(".hidden"));
        assert!(!valid_name("a/b"));
        assert!(!valid_name(&"x".repeat(65)));
    }

    #[test]
    fn mutating_flags() {
        assert!(!is_mutating("pane.list"));
        assert!(is_mutating("pane.send_text"));
        assert!(is_mutating("no.such.method"));
    }
}
