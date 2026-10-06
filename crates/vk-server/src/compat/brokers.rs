//! Plugin broker bindings and their lifetimes (07 §7.7, 09 §6 "Recreate the broker binding after
//! server recovery only for still-approved live invocations").
//!
//! A broker is a private 0600 socket bound server-side to `(plugin id, grant digest)`. Its
//! binding is persisted in `$RUNTIME/<session>/herdr-compat/brokers.json`, so a restarted
//! server re-issues the same socket path for invocations that are still alive and whose grant
//! still matches. Lifetimes:
//!
//! * [`Life::Process`] — actions and event hooks: the broker closes when the invocation's
//!   process exits. Children it leaves behind lose authority (no long-running declaration).
//! * [`Life::Group`] — `[[startup]]`, the manifest's long-running entrypoint: the process is a
//!   process-group leader and the broker stays while any process of that group is alive, so
//!   daemons a startup hook spawns keep their callbacks.
//! * [`Life::Pane`] — a `[[panes]]` entrypoint opened with `plugin.pane.open`: the broker lives
//!   as long as the pane.
//!
//! A watcher per binding polls liveness and the grant every second and closes the broker when
//! either is gone. Closing a broker also aborts every connection it accepted (requests in
//! flight, `events.subscribe` streams, `events.wait`), so a child that connected before its
//! action exited keeps no authority. Every request re-checks the binding's liveness and its
//! exact grant (`grant_id`, so a revoke followed by a new grant never revives a connection).

use super::{Caller, compat_root, private_dir, serve_wire, state};
use crate::Server;
use crate::api::Ctx;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use vk_compat::herdr::registry::{self, Registry, Status};

/// What keeps a broker binding alive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Life {
    /// Bound, the process not started yet.
    Pending,
    /// Until this process exits.
    Process { pid: u32 },
    /// Until no process of this process group is left (declared long-running).
    Group { pid: u32, pgid: u32 },
    /// Until this pane (Vibeke id) is closed or its process has exited.
    Pane { pane: String },
}

/// One persisted broker binding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Binding {
    pub path: PathBuf,
    pub plugin_id: String,
    pub digest: String,
    /// Herdr pane id used when a request omits `pane_id`.
    pub default_pane: Option<String>,
    /// `startup[0]`, an action id, an event hook id or a pane entrypoint id.
    pub entrypoint: Option<String>,
    /// `api`, `cli`, `event`, `startup`, `pane`.
    pub source: String,
    pub log_id: Option<String>,
    /// The grant this invocation was started under ([`registry::Grant::grant_id`]).
    #[serde(default)]
    pub grant_id: String,
    pub life: Life,
    pub created_at_ms: i64,
    /// Output files of the invocation (actions, hooks, startup), tailed into its log record.
    #[serde(default)]
    pub stdout: Option<PathBuf>,
    #[serde(default)]
    pub stderr: Option<PathBuf>,
}

impl Binding {
    pub fn caller(&self) -> Caller {
        Caller {
            ctx: Ctx {
                client_id: format!("plugin:{}", self.plugin_id),
                kind: "plugin".into(),
                pane_scope: None,
                remote: false,
            },
            plugin: Some((self.plugin_id.clone(), self.digest.clone())),
            default_pane: self.default_pane.clone(),
            invocation: self.log_id.clone(),
            broker: Some(self.path.clone()),
            grant_id: Some(self.grant_id.clone()),
            cross_session: false,
        }
    }
}

/// A live binding: its record, the accept loop and the connections it accepted.
pub struct Live {
    pub binding: Binding,
    task: tokio::task::AbortHandle,
    conns: Arc<Mutex<Vec<tokio::task::AbortHandle>>>,
}

fn store_path(server: &Server) -> PathBuf {
    compat_root(server).join("brokers.json")
}

/// A fresh broker socket path in the session's private broker directory.
pub fn new_path(server: &Server) -> std::io::Result<PathBuf> {
    let dir = compat_root(server).join("brokers");
    private_dir(&compat_root(server))?;
    private_dir(&dir)?;
    Ok(dir.join(format!("{}.sock", &crate::core::ulid()[14..])))
}

fn persist(server: &Server) {
    let st = state(server);
    let list: Vec<Binding> = st
        .bindings
        .lock()
        .unwrap()
        .values()
        .map(|l| l.binding.clone())
        .collect();
    let path = store_path(server);
    let tmp = path.with_extension("json.tmp");
    let Ok(bytes) = serde_json::to_vec_pretty(&list) else {
        return;
    };
    use std::os::unix::fs::OpenOptionsExt;
    let ok = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| std::io::Write::write_all(&mut f, &bytes));
    if ok.is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Bind the socket for `b` and start serving it with the binding's identity.
pub fn bind(server: &Arc<Server>, b: Binding) -> std::io::Result<()> {
    let listener = super::bind_socket(&b.path)?;
    let srv = server.clone();
    let path = b.path.clone();
    let conns: Arc<Mutex<Vec<tokio::task::AbortHandle>>> = Arc::default();
    let held = conns.clone();
    let task = tokio::spawn(async move {
        loop {
            let Ok((s, _)) = listener.accept().await else {
                return;
            };
            if !super::same_uid(&s) {
                continue;
            }
            // The identity comes from the server-side binding as it is now.
            let Some(c) = get(&srv, &path).map(|b| b.caller()) else {
                return;
            };
            let srv = srv.clone();
            let h = tokio::spawn(async move { serve_wire(srv, s, c).await }).abort_handle();
            let mut v = held.lock().unwrap();
            v.retain(|x| !x.is_finished());
            v.push(h);
        }
    })
    .abort_handle();
    let path = b.path.clone();
    state(server).bindings.lock().unwrap().insert(
        path.clone(),
        Live {
            binding: b,
            task: task.clone(),
            conns,
        },
    );
    persist(server);
    let srv = server.clone();
    tokio::spawn(async move { watch(srv, path).await });
    Ok(())
}

/// Attach the binding to its process or pane once known.
pub fn set_life(server: &Server, path: &Path, life: Life) {
    if let Some(l) = state(server).bindings.lock().unwrap().get_mut(path) {
        l.binding.life = life;
    }
    persist(server);
}

/// Set the pane used when a request through this broker omits `pane_id`. The accept loop
/// carries the identity it was bound with, so it is re-bound with the new default.
pub fn set_default_pane(server: &Server, path: &Path, pane: Option<String>) {
    if let Some(l) = state(server).bindings.lock().unwrap().get_mut(path) {
        l.binding.default_pane = pane;
    }
    persist(server);
}

/// Close a broker: stop accepting, drop every accepted connection (in-flight requests,
/// subscriptions, waits), remove the socket and the binding.
pub fn close(server: &Server, path: &Path) {
    let live = state(server).bindings.lock().unwrap().remove(path);
    if let Some(l) = live {
        l.task.abort();
        for c in l.conns.lock().unwrap().drain(..) {
            c.abort();
        }
    }
    let _ = std::fs::remove_file(path);
    persist(server);
}

pub fn get(server: &Server, path: &Path) -> Option<Binding> {
    state(server)
        .bindings
        .lock()
        .unwrap()
        .get(path)
        .map(|l| l.binding.clone())
}

pub fn all(server: &Server) -> Vec<Binding> {
    state(server)
        .bindings
        .lock()
        .unwrap()
        .values()
        .map(|l| l.binding.clone())
        .collect()
}

/// `kill(pid, 0)`: the process (or, for a negative pid, the process group) exists.
pub fn pid_alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks existence/permission; no signal is delivered.
    let r = unsafe { libc::kill(pid, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

pub(super) fn alive(server: &Server, life: &Life) -> bool {
    match life {
        Life::Pending => true,
        Life::Process { pid } => pid_alive(*pid as i32),
        Life::Group { pid, pgid } => pid_alive(*pid as i32) || pid_alive(-(*pgid as i32)),
        Life::Pane { pane } => server.with_core(|c| c.pane(pane).is_some_and(|p| !p.exited)),
    }
}

/// The plugin is still registered, enabled and trusted under the very same grant (manifest
/// digest and grant id).
pub fn grant_ok(plugin: &str, digest: &str, grant_id: &str) -> bool {
    Registry::load(&super::plugin_dirs()).is_ok_and(|reg| {
        reg.get(plugin).ok().is_some_and(|e| {
            matches!(registry::entry_status(e), (Status::Active, _))
                && e.trust.as_ref().is_some_and(|g| {
                    g.manifest_sha256 == digest && !grant_id.is_empty() && g.grant_id == grant_id
                })
        })
    })
}

/// The broker at `path` is open and its invocation still alive.
pub fn live(server: &Server, path: &Path) -> Option<Binding> {
    get(server, path).filter(|b| alive(server, &b.life))
}

async fn watch(server: Arc<Server>, path: PathBuf) {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tick.tick().await;
        let Some(b) = get(&server, &path) else {
            return;
        };
        if !alive(&server, &b.life) || !grant_ok(&b.plugin_id, &b.digest, &b.grant_id) {
            tracing::debug!(broker = %path.display(), plugin = %b.plugin_id, "broker closed");
            close(&server, &path);
            if let Life::Pane { pane } = &b.life {
                super::ext::plugin_pane_gone(&server, pane);
            }
            return;
        }
    }
}

/// Re-issue persisted bindings after a server (re)start: only for invocations that are still
/// alive and whose grant still matches; everything else is dropped and its socket removed.
/// Returns `(recovered, dropped)`.
pub fn recover(server: &Arc<Server>) -> (usize, usize) {
    let path = store_path(server);
    let list: Vec<Binding> = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let (mut ok, mut dropped) = (0, 0);
    for b in list {
        let keep = !matches!(b.life, Life::Pending)
            && alive(server, &b.life)
            && grant_ok(&b.plugin_id, &b.digest, &b.grant_id)
            && b.path.starts_with(compat_root(server));
        if !keep {
            let _ = std::fs::remove_file(&b.path);
            dropped += 1;
            continue;
        }
        let resume = match &b.life {
            Life::Process { pid } | Life::Group { pid, .. } => Some(*pid),
            _ => None,
        };
        let (log, out, err) = (b.log_id.clone(), b.stdout.clone(), b.stderr.clone());
        let pane = match (&b.life, b.source.strip_prefix("pane:")) {
            (Life::Pane { pane }, Some(placement)) => Some((
                pane.clone(),
                super::ext::PluginPane {
                    plugin: b.plugin_id.clone(),
                    entrypoint: b.entrypoint.clone().unwrap_or_default(),
                    placement: placement.to_string(),
                    prev_focus: None,
                },
            )),
            _ => None,
        };
        match bind(server, b) {
            Ok(()) => {
                ok += 1;
                if let Some((id, pp)) = pane {
                    state(server)
                        .meta
                        .lock()
                        .unwrap()
                        .plugin_panes
                        .insert(id, pp);
                }
                if let (Some(pid), Some(log)) = (resume, log) {
                    super::resume_tail(server, log, out, err, pid);
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "broker not re-issued");
                dropped += 1;
            }
        }
    }
    persist(server);
    (ok, dropped)
}
