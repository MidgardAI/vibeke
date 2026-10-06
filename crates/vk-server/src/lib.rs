#![allow(clippy::result_large_err)]
//! The Vibeke server (01 §1.3): one process per (machine, session). It owns the layout and
//! session state (SQLite + outbox), connects to per-pane holders, runs VT engines, serves the
//! JSON-RPC control API and per-client render streams, and hosts agent adapters.

pub mod agent_browser;
pub mod agents;
pub mod api;
pub mod api_schema;
pub mod assist;
pub mod blob_api;
pub mod blob_store;
pub mod browser_pane;
pub mod compat;
pub mod config_api;
pub mod core;
pub mod desk;
pub mod drafts;
pub mod fs_api;
pub mod gateway_api;
pub mod git_api;
pub mod hardening;
pub mod inbox;
pub mod items;
pub mod layouts;
pub mod limits;
pub mod machines;
pub mod notify;
pub mod orch;
pub mod orch_family;
pub mod orch_goal;
pub mod orch_learn;
pub mod orch_merge;
pub mod orch_quota;
pub mod orch_shapes;
pub mod orch_split;
pub mod orch_vm;
pub mod pane;
pub mod pane_api;
pub mod parity;
pub mod paths;
pub mod preview;
pub mod preview_ca;
pub mod preview_console;
pub mod preview_fabric;
pub mod render;
pub mod repo_config;
pub mod review;
pub mod run;
pub mod sandbox;
pub mod screenshots;
pub mod search;
pub mod session_api;
pub mod shape;
pub mod task_park;
pub mod task_workspace;
pub mod term_effects;
pub mod theme;
pub mod timers;
pub mod tracking;
// Server security (09): audit log, auth, policy, integration tamper detection, debug bundle.
pub mod audit;
pub mod auth;
pub mod debug_bundle;
pub mod integrity;
pub mod policy_api;
pub mod security;
pub mod user_popup;
// v1 server/API/CLI remainder: one module per item, hooked with one line each.
pub mod audit_retention;
pub mod pane_render;
pub mod sync_input;
pub mod tab_renumber;
pub mod task_lifecycle;
// Lane 3E: encryption at rest, index redaction, forget coverage (09 §9).
pub mod forget_scope;
pub mod privacy;

#[cfg(test)]
mod scope_catalog_tests;
#[cfg(test)]
mod scrollback_tests;
#[cfg(test)]
mod security_tests;

use crate::core::{Core, Tx, subject_pane, ulid};
use crate::pane::{HolderConn, PaneCmd, PaneRt};
use crate::paths::Paths;
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::{Notify, broadcast, watch};
use vk_proto::holder::{ProcStatus, SpawnSpec};
use vk_proto::layout::{self, Direction};
use vk_proto::model::*;
use vk_store::archive::{Archive, ArchivedRow};
use vk_store::{Event, now_ms};
use vk_term::{Effect, NotifyKind};

#[derive(Debug, Clone)]
pub struct ServerOpts {
    pub session: String,
    pub machine: String,
    /// Binary used to spawn holders (`<bin> hold …`) and referenced as `VIBEKE_BIN`.
    pub bin: PathBuf,
    pub hold_args: Vec<String>,
    pub default_shell: Option<String>,
    pub env: Vec<(String, String)>,
    pub shims: bool,
}

/// Out-of-band UI events for attached render clients.
#[derive(Debug, Clone)]
pub enum UiEvent {
    Bell {
        pane: String,
    },
    Clipboard {
        pane: String,
        primary: bool,
        data: Vec<u8>,
    },
    Notify(Notification),
    Goodbye(String),
    /// OSC 52 read (03 §8): only the render session of `client` forwards it.
    ClipboardQuery {
        client: String,
        req: u64,
        pane: String,
        primary: bool,
    },
}

#[derive(Debug, Clone, Default)]
pub struct ClientState {
    pub kind: String,
    pub focus: ClientFocus,
    pub last_active: Option<Instant>,
    pub attached_at_ms: i64,
    pub visible: Vec<String>,
    pub host_focused: bool,
}

pub struct Server {
    pub paths: Paths,
    pub opts: ServerOpts,
    pub core: Mutex<Core>,
    pub panes: Mutex<HashMap<String, Arc<PaneRt>>>,
    pub model_rev: watch::Sender<u64>,
    pub events: broadcast::Sender<Arc<Event>>,
    pub ui: broadcast::Sender<UiEvent>,
    pub screen_dirty: Notify,
    pub clients: Mutex<HashMap<String, ClientState>>,
    pub geometry_leader: Mutex<Option<String>>,
    /// Leased-port env of tasks being created (their first pane spawns before the task is in
    /// the model), by task id.
    pub pending_task_env: Mutex<HashMap<String, Vec<(String, String)>>>,
    pub boot_id: String,
    pub started: Instant,
    pub archive: Mutex<Archive>,
    /// Rows waiting for the next FTS flush. Lock order: `archive` → `core` → `fts_buf`; the
    /// batch is drained and indexed under the archive lock, which purges also hold.
    pub fts_buf: Mutex<Vec<(String, u64, i64, String)>>,
    /// Test hook: runs in [`Server::housekeeping`] right after the FTS batch was drained.
    #[cfg(test)]
    pub(crate) after_fts_drain: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    pub tokens: Mutex<HashMap<String, String>>,
    pub tracking: tracking::State,
    pub gateway: gateway_api::State,
    pub agents: agents::Agents,
    pub previews: preview::Previews,
    /// Execution isolation contexts (13).
    pub sandbox: sandbox::State,
    pub agent_browser: agent_browser::AgentBrowsers,
    /// Browser panes rendered on this machine (06 B3.2).
    pub browser: browser_pane::Host,
    /// Native notifier, client host terminals, coalescing (08 §7).
    pub notifier: notify::State,
    /// Host appearance reports and the effective theme (08 §11 `[theme]`).
    pub theme: theme::State,
    /// Terminal effects state: open OSC 52 read queries (03 §8).
    pub term_fx: term_effects::State,
    /// Session desk conversation index (research R2).
    pub desk: desk::State,
    /// Drafts composer in-flight sends (research R3).
    pub drafts: drafts::State,
    /// User-invoked LLM drafts (14).
    pub assist: assist::State,
    pub shutdown: Notify,
    input_counter: AtomicU64,
    pub degraded: Mutex<Option<String>>,
    /// Coalesced pane deadlines (spec 10 §1.3 wakeup budget).
    pub timers: Arc<timers::Scheduler>,
    /// Wakes the housekeeping task: archive rows to flush, or storage degraded.
    pub housekeeping_wake: Notify,
    pub housekeeping_runs: AtomicU64,
    /// Audit log, token revocation and elevation, integration tamper state (09).
    pub security: security::State,
    /// Encryption at rest and redaction settings (09 §9.1–9.2).
    pub privacy: privacy::State,
    /// Degraded-mode bookkeeping, storage sweeps (02 §4a, 3D).
    pub hardening: hardening::State,
    /// Turn/Item stream recorder (02 §1.1, 3D).
    pub items: items::State,
}

pub fn shell_argv(opts: &ServerOpts) -> Vec<String> {
    let shell = opts
        .default_shell
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("SHELL").ok())
        .unwrap_or_else(|| "/bin/sh".into());
    vec![shell, "-l".into()]
}

fn token_hash(token: &str) -> String {
    blake3::hash(token.as_bytes()).to_hex().to_string()
}

impl Server {
    pub fn new(paths: Paths, opts: ServerOpts) -> Result<Arc<Self>> {
        paths.ensure()?;
        let store = vk_store::Store::open(&paths.db())?;
        // Settle archive purges a crash interrupted (02 "Archive search as implemented").
        match store.recover_archive_purges(&paths.scrollback()) {
            Ok(r) if r != vk_store::PurgeRecovery::default() => {
                tracing::warn!(?r, "archive: settled interrupted purges");
            }
            Ok(_) => {}
            Err(e) => tracing::error!(error = %e, "archive: purge recovery failed"),
        }
        let mut core = Core::load(store, &opts.session, &opts.machine)?;
        // Pane tokens are stored as blake3 hashes only (09 §3.2). Migrate the old raw record.
        let mut tokens: HashMap<String, String> = core
            .store
            .kv_get("server", "pane_token_hashes")?
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        if let Some(raw) = core
            .store
            .kv_get("server", "pane_tokens")?
            .and_then(|s| serde_json::from_str::<HashMap<String, String>>(&s).ok())
        {
            for (tok, pane) in raw {
                tokens.insert(token_hash(&tok), pane);
            }
            let mut m = vk_store::Mutation::default();
            m.kv(
                "server",
                "pane_token_hashes",
                Some(serde_json::to_string(&tokens).unwrap_or_default()),
            )
            .kv("server", "pane_tokens", None);
            core.store.commit(m)?;
        }
        let (model_rev, _) = watch::channel(1);
        let (events, _) = broadcast::channel(4096);
        let (ui, _) = broadcast::channel(256);
        let server = Arc::new(Server {
            archive: Mutex::new(Archive::new(&paths.scrollback())),
            paths,
            opts,
            core: Mutex::new(core),
            panes: Mutex::new(HashMap::new()),
            model_rev,
            events,
            ui,
            screen_dirty: Notify::new(),
            clients: Mutex::new(HashMap::new()),
            geometry_leader: Mutex::new(None),
            pending_task_env: Mutex::new(HashMap::new()),
            boot_id: ulid(),
            started: Instant::now(),
            fts_buf: Mutex::new(Vec::new()),
            #[cfg(test)]
            after_fts_drain: Mutex::new(None),
            tokens: Mutex::new(tokens),
            tracking: Default::default(),
            gateway: Default::default(),
            agents: agents::Agents::default(),
            previews: preview::Previews::default(),
            sandbox: sandbox::State::default(),
            agent_browser: agent_browser::AgentBrowsers::default(),
            browser: browser_pane::Host::default(),
            notifier: notify::State::default(),
            theme: theme::State::default(),
            term_fx: term_effects::State::default(),
            desk: Default::default(),
            drafts: Default::default(),
            assist: assist::State::default(),
            shutdown: Notify::new(),
            input_counter: AtomicU64::new(rand::random::<u32>() as u64),
            degraded: Mutex::new(None),
            timers: Arc::default(),
            housekeeping_wake: Notify::new(),
            housekeeping_runs: AtomicU64::new(0),
            security: Default::default(),
            privacy: Default::default(),
            hardening: Default::default(),
            items: Default::default(),
        });
        // Unlock the state key before anything is archived (09 §9.1).
        privacy::init(&server);
        Ok(server)
    }

    pub fn with_core<T>(&self, f: impl FnOnce(&mut Core) -> T) -> T {
        f(&mut self.core.lock().unwrap())
    }

    /// Commit a transaction, publish its events and bump the model revision.
    pub fn commit(&self, core: &mut Core, tx: Tx) -> Result<Vec<Event>> {
        match core.commit(tx) {
            Ok(events) => {
                if let Some(msg) = core.ephemeral_hit.take() {
                    // The store refused the write and the transaction was a UI convenience
                    // (focus, unread marks): applied in memory only (02 §4a).
                    self.hardening.note_ephemeral();
                    self.enter_degraded(core, msg);
                    return Ok(events);
                }
                *self.degraded.lock().unwrap() = None;
                core.model.degraded = None;
                self.hardening.recovered();
                for e in &events {
                    let _ = self.events.send(Arc::new(e.clone()));
                }
                self.bump_model();
                Ok(events)
            }
            Err(e) => {
                self.enter_degraded(core, format!("storage unavailable: {e:#}"));
                Err(e)
            }
        }
    }

    /// Mark the server degraded (02 §4a): housekeeping probes storage until it recovers.
    pub fn enter_degraded(&self, core: &mut Core, msg: String) {
        *self.degraded.lock().unwrap() = Some(msg.clone());
        self.housekeeping_wake.notify_one();
        core.model.degraded = Some(msg);
        self.bump_model();
    }

    pub fn bump_model(&self) {
        self.model_rev.send_modify(|r| *r += 1);
    }

    /// Unique ids for server-originated writes (query replies), disjoint from client ids.
    pub fn next_internal_input_id(&self) -> u64 {
        (1u64 << 63) | self.input_counter.fetch_add(1, Ordering::Relaxed)
    }

    pub fn pane_rt(&self, id: &str) -> Option<Arc<PaneRt>> {
        self.panes.lock().unwrap().get(id).cloned()
    }

    /// Mint a token for a pane's (new) environment. Earlier tokens for the pane are revoked:
    /// only the process tree started with this environment holds a valid one.
    pub fn token_for(&self, pane: &str) -> String {
        let mut t = self.tokens.lock().unwrap();
        t.retain(|_, p| p != pane);
        let tok: String = (0..32)
            .map(|_| format!("{:02x}", rand::random::<u8>()))
            .collect();
        t.insert(token_hash(&tok), pane.to_string());
        tok
    }

    pub fn pane_for_token(&self, token: &str) -> Option<String> {
        self.tokens.lock().unwrap().get(&token_hash(token)).cloned()
    }

    fn persist_tokens(&self, tx: &mut Tx) {
        let t = self.tokens.lock().unwrap();
        tx.m.kv(
            "server",
            "pane_token_hashes",
            Some(serde_json::to_string(&*t).unwrap_or_default()),
        );
    }

    // ---- startup / recovery ---------------------------------------------------------------

    /// Reattach to every live holder; panes whose holder is gone are recreated (reboot) or
    /// closed. Returns the number of recovered panes.
    pub fn recover(self: &Arc<Self>) -> Result<usize> {
        tracking::recover(self);
        review::recover(self);
        let holders = self.with_core(|c| c.store.holders())?;
        let panes: Vec<Pane> = self.with_core(|c| c.model.panes.clone());
        let mut recovered = 0;
        let mut lost = Vec::new();
        for p in &panes {
            if p.is_browser() {
                // No holder: the viewing client's server re-creates the page (06 B3.2).
                continue;
            }
            let h = holders.iter().find(|h| h.pane == p.id);
            // Alive = its socket accepts, or its recorded pid is still a holder (a busy or
            // briefly unreachable holder must be reattached, never respawned over).
            let alive = h.is_some_and(|h| {
                vk_hold::holder_alive(std::path::Path::new(&h.socket), h.holder_pid)
            });
            match (h, alive) {
                (Some(h), true) => {
                    let (rt, rx) = PaneRt::new(&p.id, p.cols.max(2), p.rows.max(1));
                    self.panes.lock().unwrap().insert(p.id.clone(), rt.clone());
                    let conn = HolderConn {
                        socket: h.socket.clone(),
                        key: h.key.clone(),
                        epoch: h.epoch,
                        fresh: false,
                    };
                    tokio::spawn(pane::run(self.clone(), rt, rx, conn));
                    recovered += 1;
                }
                _ => lost.push(p.clone()),
            }
        }
        // Reboot (or holder crash): respawn shells in the old layout positions and offer resume.
        for p in lost {
            let cwd = p
                .cwd
                .clone()
                .unwrap_or_else(|| paths::home().to_string_lossy().into_owned());
            let run = self.with_core(|c| c.run_for_pane(&p.id).cloned());
            match self.respawn_pane(&p, &cwd) {
                Ok(()) => {
                    if let Some(r) = run {
                        self.agents.end_run(self, &r.id, "holder_lost");
                    }
                }
                Err(e) => {
                    // Keep the slot, marked exited/lost, rather than dropping it.
                    tracing::warn!(pane = %p.id, error = %e, "respawn failed");
                    let mut c = self.core.lock().unwrap();
                    if let Some(mut q) = c.pane(&p.id).cloned() {
                        q.exited = true;
                        q.recovered = Some("lost".into());
                        let mut tx = Tx::new();
                        tx.event(
                            "pane.exited",
                            subject_pane(&q),
                            json!({"code": null, "signal": null, "reason": "holder_lost", "respawn_error": format!("{e:#}")}),
                        );
                        tx.pane(q);
                        let _ = self.commit(&mut c, tx);
                    }
                }
            }
        }
        let mut c = self.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.event(
            "session.server_restarted",
            json!({}),
            json!({"recovered_panes": recovered, "pid": std::process::id()}),
        );
        let _ = self.commit(&mut c, tx);
        Ok(recovered)
    }

    /// A holder vanished while this server was attached to it. Called only once the holder
    /// process is known to be gone (a dropped connection to a live holder reconnects in
    /// `pane::run`). Respawns a shell in the same layout slot; if that fails the pane stays
    /// in the layout marked exited/lost, so nothing is silently removed.
    pub fn holder_lost(self: &Arc<Self>, pane_id: &str) {
        self.panes.lock().unwrap().remove(pane_id);
        let Some(p) = self.with_core(|c| c.pane(pane_id).cloned()) else {
            return;
        };
        let rec = self
            .with_core(|c| c.store.holders())
            .ok()
            .and_then(|hs| hs.into_iter().find(|h| h.pane == pane_id));
        if let Some(h) = rec
            && vk_hold::holder_alive(std::path::Path::new(&h.socket), h.holder_pid)
        {
            // Defensive: never respawn over a live holder (its socket would refuse the new
            // one and its agent would be orphaned). Reattach instead.
            tracing::warn!(pane = %pane_id, "holder still alive; reattaching instead of respawning");
            self.start_pane(pane_id, p.cols, p.rows, h.socket, h.key, false);
            return;
        }
        if let Some(r) = self.with_core(|c| c.run_for_pane(pane_id).cloned()) {
            self.agents.end_run(self, &r.id, "holder_lost");
        }
        let cwd = p
            .cwd
            .clone()
            .unwrap_or_else(|| paths::home().to_string_lossy().into_owned());
        if let Err(e) = self.respawn_pane(&p, &cwd) {
            tracing::warn!(pane = %pane_id, error = %e, "respawn after holder loss failed");
            let mut c = self.core.lock().unwrap();
            if let Some(mut q) = c.pane(pane_id).cloned() {
                q.exited = true;
                q.recovered = Some("lost".into());
                let mut tx = Tx::new();
                tx.event(
                    "pane.exited",
                    subject_pane(&q),
                    json!({"code": null, "signal": null, "reason": "holder_lost", "respawn_error": format!("{e:#}")}),
                );
                tx.pane(q);
                let _ = self.commit(&mut c, tx);
            }
            drop(c);
            self.notify(
                "system",
                Some(pane_id),
                "pane lost",
                "its process was lost and a new shell could not be started; close the pane",
                "normal",
            );
        } else {
            self.notify(
                "system",
                Some(pane_id),
                "pane restarted",
                "its process was lost; agents can be resumed (vibeke agent resumable)",
                "normal",
            );
        }
    }

    fn respawn_pane(self: &Arc<Self>, old: &Pane, cwd: &str) -> Result<()> {
        let argv = shell_argv(&self.opts);
        let (wsh, tabh, ws_task, task_env) = self.with_core(|c| {
            let ws_task = c.ws(&old.workspace).and_then(|w| w.task.clone());
            let task_env = self.task_env_for(c, ws_task.as_deref());
            (
                c.ws(&old.workspace)
                    .map(|w| w.handle.clone())
                    .unwrap_or_default(),
                c.tab(&old.tab)
                    .map(|t| t.handle.clone())
                    .unwrap_or_default(),
                ws_task,
                task_env,
            )
        });
        let (holder_pid, child_pid, socket, key, isolation) = self.spawn_holder(
            &old.id,
            &old.handle,
            &tabh,
            &wsh,
            &argv,
            cwd,
            old.cols,
            old.rows,
            ws_task.as_deref(),
            task_env,
        )?;
        let mut c = self.core.lock().unwrap();
        let mut p = old.clone();
        p.isolation = isolation;
        p.child_pid = Some(child_pid);
        p.exited = false;
        p.recovered = Some("lost".into());
        let mut tx = Tx::new();
        tx.m.holder(&p.id, &socket, &key, 0, Some(holder_pid), Some(child_pid));
        // The old holder's VT snapshot describes a screen and ring offsets that no longer
        // exist; a later recovery must not restore it.
        tx.m.snapshot_delete(&p.id);
        tx.event(
            "pane.recovered",
            subject_pane(&p),
            json!({"method": "lost"}),
        );
        tx.pane(p.clone());
        self.commit(&mut c, tx)?;
        drop(c);
        self.start_pane(&p.id, p.cols, p.rows, socket, key, true);
        Ok(())
    }

    // ---- spawning -------------------------------------------------------------------------

    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    fn spawn_holder(
        self: &Arc<Self>,
        pane_id: &str,
        handle: &str,
        tab_handle: &str,
        ws_handle: &str,
        argv: &[String],
        cwd: &str,
        cols: u16,
        rows: u16,
        ws_task: Option<&str>,
        task_env: Vec<(String, String)>,
    ) -> Result<(u32, u32, String, Vec<u8>, Isolation)> {
        let socket = self
            .paths
            .holder_socket(pane_id)
            .to_string_lossy()
            .into_owned();
        let key: Vec<u8> = (0..32).map(|_| rand::random::<u8>()).collect();
        let env = self.pane_env(pane_id, handle, tab_handle, ws_handle, &task_env);
        let cwd = if std::path::Path::new(cwd).is_dir() {
            cwd.to_string()
        } else {
            paths::home().to_string_lossy().into_owned()
        };
        // Headless harnesses run under a pipe-mode holder (01 §1.2).
        let (mode, argv) = pane::holder_mode(argv);
        // Execution isolation (13): a sandboxed task's panes get a wrapped command and env; an
        // isolated headless run spawns inside the run-scoped box prepared for its pane.
        let run_box = if mode == vk_proto::holder::Mode::Pipe {
            sandbox::wrap_run_spawn(self, pane_id, &cwd, argv, env.clone())
                .context("prepare isolated spawn")?
        } else {
            None
        };
        let (argv, env, isolation) = match run_box {
            Some(w) => w,
            None => sandbox::wrap_spawn(self, pane_id, &cwd, argv, env, ws_task)
                .context("prepare isolated spawn")?,
        };
        let spec = SpawnSpec {
            pane_id: pane_id.to_string(),
            socket: socket.clone(),
            argv,
            cwd,
            env,
            key: key.clone(),
            cols: cols.max(2),
            rows: rows.max(1),
            ring_bytes: 16 << 20,
            mode,
        };
        let args: Vec<&str> = self.opts.hold_args.iter().map(String::as_str).collect();
        let log = self.paths.logs().join(format!("holder-{pane_id}.log"));
        let l = vk_hold::launch(
            &self.opts.bin,
            &args,
            &spec,
            &self.paths.runtime,
            Some(&log),
        )
        .context("launch holder")?;
        Ok((l.holder_pid, l.child_pid, socket, key, isolation))
    }

    /// The env a host pane would get (sandbox launches scrub and extend it, 13 §8).
    pub(crate) fn pane_env_for(
        &self,
        pane_id: &str,
        handle: &str,
        tab_handle: &str,
        ws_handle: &str,
        task_env: &[(String, String)],
    ) -> Vec<(String, String)> {
        self.pane_env(pane_id, handle, tab_handle, ws_handle, task_env)
    }

    /// Leased-port env for panes of task `ws_task` (empty for non-task workspaces).
    pub(crate) fn task_env_for(&self, c: &Core, ws_task: Option<&str>) -> Vec<(String, String)> {
        let Some(id) = ws_task else {
            return vec![];
        };
        if let Some(e) = self.pending_task_env.lock().unwrap().get(id) {
            return e.clone();
        }
        let mut env = c
            .task(id)
            .map(preview_fabric::task_port_env)
            .unwrap_or_default();
        // `VIBEKE_TASK_SLUG`, `[env]` and `[ports] env` of the task file, saved at creation.
        let saved: Vec<(String, String)> = c
            .store
            .kv_get("task_env", id)
            .ok()
            .flatten()
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_default();
        for (k, v) in saved {
            env.retain(|(n, _)| n != &k);
            env.push((k, v));
        }
        env
    }

    /// Must not lock `core`: callers hold it while spawning.
    fn pane_env(
        &self,
        pane_id: &str,
        handle: &str,
        tab_handle: &str,
        ws_handle: &str,
        task_env: &[(String, String)],
    ) -> Vec<(String, String)> {
        let mut env: Vec<(String, String)> = self
            .opts
            .env
            .iter()
            // Never leak an outer Herdr/Vibeke pane identity into our panes (would make Herdr's
            // hooks report into the real Herdr session).
            .filter(|(k, _)| {
                !k.starts_with("HERDR_")
                    && !k.starts_with("VIBEKE_")
                    && k != "VIBEKE"
                    && k != "TMUX"
                    && k != "TMUX_PANE"
            })
            .cloned()
            .collect();
        let set = |env: &mut Vec<(String, String)>, k: &str, v: String| {
            env.retain(|(x, _)| x != k);
            env.push((k.to_string(), v));
        };
        set(&mut env, "TERM", "xterm-256color".into());
        set(&mut env, "COLORTERM", "truecolor".into());
        set(&mut env, "TERM_PROGRAM", "vibeke".into());
        set(&mut env, "TERM_PROGRAM_VERSION", vk_proto::VERSION.into());
        set(&mut env, "VIBEKE", "1".into());
        set(
            &mut env,
            "VIBEKE_SOCKET",
            self.paths.socket().to_string_lossy().into_owned(),
        );
        set(&mut env, "VIBEKE_PANE_ID", handle.into());
        set(&mut env, "VIBEKE_PANE_ULID", pane_id.into());
        set(&mut env, "VIBEKE_WORKSPACE_ID", ws_handle.to_string());
        set(&mut env, "VIBEKE_TAB_ID", tab_handle.to_string());
        set(&mut env, "VIBEKE_SESSION", self.opts.session.clone());
        set(
            &mut env,
            "VIBEKE_BIN",
            self.opts.bin.to_string_lossy().into_owned(),
        );
        set(&mut env, "VIBEKE_PANE_TOKEN", self.token_for(pane_id));
        // Vibeke-only enforcement fails closed in the pre-tool hook of a yolo run (04 §2.7).
        if vk_config::Config::load(vk_config::config_path())
            .map(|(c, _)| c.agents.fail_closed)
            .unwrap_or(true)
        {
            set(&mut env, "VIBEKE_ENFORCE", "1".into());
        }
        theme::pane_env(self, &mut env);
        // `HERDR_*` aliases when `compat.herdr_env` and the compat listener are on (07 §8.2).
        compat::extend_pane_env(self, &mut env, handle, tab_handle, ws_handle);
        // Leased ports of an owned task workspace (`PORT`, `[ports] env` names).
        for (k, v) in task_env {
            set(&mut env, k, v.clone());
        }
        if self.opts.shims {
            let shims = Paths::shims();
            if shims.is_dir() {
                let path = env
                    .iter()
                    .find(|(k, _)| k == "PATH")
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default();
                set(&mut env, "PATH", format!("{}:{path}", shims.display()));
            }
        }
        env
    }

    fn start_pane(
        self: &Arc<Self>,
        id: &str,
        cols: u16,
        rows: u16,
        socket: String,
        key: Vec<u8>,
        fresh: bool,
    ) {
        let (rt, rx) = PaneRt::new(id, cols, rows);
        self.panes
            .lock()
            .unwrap()
            .insert(id.to_string(), rt.clone());
        tokio::spawn(pane::run(
            self.clone(),
            rt,
            rx,
            HolderConn {
                socket,
                key,
                epoch: 0,
                fresh,
            },
        ));
    }

    /// Spawn a pane process in an existing tab (layout insertion is done by the caller).
    #[allow(clippy::too_many_arguments)]
    fn new_pane(
        self: &Arc<Self>,
        c: &mut Core,
        tx: &mut Tx,
        ws: &Workspace,
        tab_id: &str,
        tab_handle: &str,
        cwd: &str,
        command: Option<Vec<String>>,
        title: Option<String>,
        created_by: &str,
    ) -> Result<Pane> {
        self.new_pane_as(
            c,
            tx,
            ws,
            tab_id,
            tab_handle,
            cwd,
            command,
            title,
            created_by,
            ulid(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new_pane_as(
        self: &Arc<Self>,
        c: &mut Core,
        tx: &mut Tx,
        ws: &Workspace,
        tab_id: &str,
        tab_handle: &str,
        cwd: &str,
        command: Option<Vec<String>>,
        title: Option<String>,
        created_by: &str,
        id: String,
    ) -> Result<Pane> {
        let handle = c.next_pane_handle(&ws.handle);
        let (cols, rows) = (80, 24);
        let argv = command.unwrap_or_else(|| shell_argv(&self.opts));
        let (holder_pid, child_pid, socket, key, isolation) = self.spawn_holder(
            &id,
            &handle,
            tab_handle,
            &ws.handle,
            &argv,
            cwd,
            cols,
            rows,
            ws.task.as_deref(),
            self.task_env_for(c, ws.task.as_deref()),
        )?;
        let pane = Pane {
            id: id.clone(),
            handle,
            tab: tab_id.to_string(),
            workspace: ws.id.clone(),
            title,
            auto_title: argv
                .first()
                .map(|a| a.rsplit('/').next().unwrap_or(a).to_string())
                .unwrap_or_default(),
            cwd: Some(cwd.to_string()),
            cols,
            rows,
            child_pid: Some(child_pid),
            fg_cmdline: vec![],
            exited: false,
            exit_code: None,
            unread: false,
            marked_unread: false,
            pinned: false,
            created_by: created_by.into(),
            recovered: None,
            isolation,
            browser: None,
        };
        tx.m.holder(&id, &socket, &key, 0, Some(holder_pid), Some(child_pid));
        tx.event(
            "pane.created",
            subject_pane(&pane),
            json!({"cwd": cwd, "command": argv}),
        );
        tx.counters = true;
        self.persist_tokens(tx);
        tx.pane(pane.clone());
        self.start_pane(&id, cols, rows, socket, key, true);
        Ok(pane)
    }

    pub fn create_workspace(
        self: &Arc<Self>,
        cwd: &str,
        name: Option<String>,
        command: Option<Vec<String>>,
        focus_client: Option<&str>,
    ) -> Result<(Workspace, Tab, Pane)> {
        self.create_workspace_for(cwd, name, command, focus_client, None)
    }

    /// [`Self::create_workspace`] for a task's workspace: its first pane already gets the
    /// task's leased-port env (the task is registered in the model afterwards).
    pub fn create_workspace_for(
        self: &Arc<Self>,
        cwd: &str,
        name: Option<String>,
        command: Option<Vec<String>>,
        focus_client: Option<&str>,
        task: Option<&str>,
    ) -> Result<(Workspace, Tab, Pane)> {
        let mut c = self.core.lock().unwrap();
        let id = ulid();
        let handle = c.next_ws_handle();
        let auto = std::path::Path::new(cwd)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| cwd.to_string());
        let order = c
            .model
            .workspaces
            .iter()
            .map(|w| w.order)
            .fold(0.0, f64::max)
            + 1.0;
        let ws = Workspace {
            id: id.clone(),
            handle,
            name,
            auto_name: auto,
            root_path: cwd.to_string(),
            task: task.map(str::to_string),
            order,
            branch: None,
        };
        let mut tx = Tx::new();
        tx.ws(ws.clone());
        tx.event(
            "workspace.created",
            json!({"workspace": ws.id}),
            json!({"cwd": cwd}),
        );
        let tab = self.tab_in(&mut c, &mut tx, &ws, None, cwd, command, None)?;
        let pane_id = tab.focused_pane.clone().unwrap_or_default();
        let pane = tx
            .panes
            .iter()
            .find(|p| p.id == pane_id)
            .cloned()
            .context("pane")?;
        self.commit(&mut c, tx)?;
        drop(c);
        if let Some(client) = focus_client {
            self.focus_pane(client, &pane.id);
        }
        Ok((ws, tab, pane))
    }

    #[allow(clippy::too_many_arguments)]
    fn tab_in(
        self: &Arc<Self>,
        c: &mut Core,
        tx: &mut Tx,
        ws: &Workspace,
        title: Option<String>,
        cwd: &str,
        command: Option<Vec<String>>,
        pane_id: Option<String>,
    ) -> Result<Tab> {
        let id = ulid();
        let number = c.next_tab_number(&ws.id);
        let order = c
            .tabs_of(&ws.id)
            .iter()
            .map(|t| t.order)
            .fold(0.0, f64::max)
            + 1.0;
        let tab_handle = format!("{}:t{number}", ws.handle);
        let pane = self.new_pane_as(
            c,
            tx,
            ws,
            &id,
            &tab_handle,
            cwd,
            command,
            None,
            "user",
            pane_id.unwrap_or_else(ulid),
        )?;
        let tab = Tab {
            id: id.clone(),
            handle: tab_handle,
            workspace: ws.id.clone(),
            title,
            number,
            layout: LayoutNode::Leaf {
                pane: pane.id.clone(),
            },
            focused_pane: Some(pane.id.clone()),
            zoomed_pane: None,
            order,
            floating: vec![],
            floats_hidden: false,
        };
        tx.event(
            "tab.created",
            json!({"tab": id, "workspace": ws.id}),
            json!({"number": number}),
        );
        tx.tab(tab.clone());
        Ok(tab)
    }

    pub fn create_tab(
        self: &Arc<Self>,
        ws_id: &str,
        cwd: Option<&str>,
        title: Option<String>,
        command: Option<Vec<String>>,
        focus_client: Option<&str>,
    ) -> Result<(Tab, Pane)> {
        self.create_tab_as(ws_id, cwd, title, command, focus_client, None)
    }

    /// [`Self::create_tab`] whose pane gets a pre-chosen id (an isolated headless run prepares
    /// its run-scoped sandbox under that id before the pane spawns).
    pub fn create_tab_as(
        self: &Arc<Self>,
        ws_id: &str,
        cwd: Option<&str>,
        title: Option<String>,
        command: Option<Vec<String>>,
        focus_client: Option<&str>,
        pane_id: Option<String>,
    ) -> Result<(Tab, Pane)> {
        let mut c = self.core.lock().unwrap();
        let ws = c.ws(ws_id).cloned().context("workspace not found")?;
        let cwd = cwd
            .map(str::to_string)
            .unwrap_or_else(|| ws.root_path.clone());
        let mut tx = Tx::new();
        let tab = self.tab_in(&mut c, &mut tx, &ws, title, &cwd, command, pane_id)?;
        let pane = tx
            .panes
            .iter()
            .find(|p| Some(&p.id) == tab.focused_pane.as_ref())
            .cloned()
            .context("pane")?;
        self.commit(&mut c, tx)?;
        drop(c);
        if let Some(client) = focus_client {
            self.focus_pane(client, &pane.id);
        }
        Ok((tab, pane))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn split_pane(
        self: &Arc<Self>,
        target: &str,
        dir: Direction,
        ratio: f32,
        cwd: Option<&str>,
        command: Option<Vec<String>>,
        title: Option<String>,
        focus_client: Option<&str>,
        created_by: &str,
    ) -> Result<Pane> {
        // Resolve the cwd before taking `core` (pane_cwd may lock it; lock order: core → clients,
        // never re-entrant).
        let target_id = self
            .with_core(|c| c.pane(target).map(|p| p.id.clone()))
            .context("pane not found")?;
        let cwd_resolved = cwd
            .map(str::to_string)
            .or_else(|| self.pane_cwd(&target_id));
        let mut c = self.core.lock().unwrap();
        let tp = c.pane(&target_id).cloned().context("pane not found")?;
        let ws = c.ws(&tp.workspace).cloned().context("workspace")?;
        let mut tab = c.tab(&tp.tab).cloned().context("tab")?;
        let cwd = cwd_resolved.unwrap_or_else(|| ws.root_path.clone());
        let mut tx = Tx::new();
        let tab_handle = tab.handle.clone();
        let pane = self.new_pane(
            &mut c,
            &mut tx,
            &ws,
            &tab.id,
            &tab_handle,
            &cwd,
            command,
            title,
            created_by,
        )?;
        layout::split(&mut tab.layout, &tp.id, &pane.id, dir, ratio);
        tab.zoomed_pane = None;
        tx.event("tab.layout_changed", json!({"tab": tab.id}), json!({}));
        tx.tab(tab);
        self.commit(&mut c, tx)?;
        drop(c);
        if let Some(client) = focus_client {
            self.focus_pane(client, &pane.id);
        }
        Ok(pane)
    }

    /// Current cwd of a pane (03 §8): OSC 7 if reported, else the foreground process's cwd
    /// read now (`/proc/<pid>/cwd` on Linux, `proc_pidinfo` on macOS; the holder runs on this
    /// machine), else the last one the holder reported, else the cwd the pane started in.
    pub fn pane_cwd(&self, pane: &str) -> Option<String> {
        let rt = self.pane_rt(pane)?;
        if let Some(c) = rt.screen.lock().unwrap().engine.cwd() {
            return Some(c.to_string());
        }
        let st = rt.status.lock().unwrap().clone();
        let live = st
            .as_ref()
            .and_then(|s| s.fg_pgid.or((s.child_pid != 0).then_some(s.child_pid)))
            .and_then(vk_hold::procinfo::cwd);
        live.or_else(|| st.and_then(|s| s.fg_cwd))
            .or_else(|| self.with_core(|c| c.pane(pane).and_then(|p| p.cwd.clone())))
    }

    /// Ask a pane's process to exit; the layout updates when the holder reports the exit.
    pub fn close_pane(&self, pane: &str) {
        if let Some(rt) = self.pane_rt(pane) {
            rt.send(PaneCmd::Close);
        } else {
            self.pane_ended(pane, "closed");
        }
    }

    /// Remove a pane whose process ended from the layout (closing empty tabs/workspaces).
    pub fn pane_ended(&self, pane_id: &str, reason: &str) {
        self.panes.lock().unwrap().remove(pane_id);
        term_effects::forget(self, pane_id);
        let had_archive = {
            let mut a = self.archive.lock().unwrap();
            let _ = a.close_pane(pane_id);
            !a.segment_infos(pane_id).is_empty()
        };
        let mut c = self.core.lock().unwrap();
        let Some(p) = c.pane(pane_id).cloned() else {
            return;
        };
        if had_archive {
            // A pane that closes within a flush interval of its last output would otherwise
            // never get its workspace recorded for scoped archive search and `forget`.
            let _ = c.store.fts_register_panes(&[(
                p.id.clone(),
                p.workspace.clone(),
                p.tab.clone(),
                p.handle.clone(),
                p.display_title().to_string(),
            )]);
        }
        let mut tx = Tx::new();
        if let Some(r) = c.run_for_pane(pane_id).cloned() {
            self.agents.end_run_tx(&mut c, &mut tx, &r, reason);
        }
        tx.close_pane(&p);
        tx.event("pane.closed", subject_pane(&p), json!({"reason": reason}));
        // Floats left behind by a tab that closes with this pane (08 §5).
        let mut orphan_floats: Vec<String> = Vec::new();
        if let Some(mut tab) = c.tab(&p.tab).cloned()
            && let Some(i) = tab.floating.iter().position(|f| f.pane == pane_id)
        {
            tab.floating.remove(i);
            if tab.focused_pane.as_deref() == Some(pane_id) {
                tab.focused_pane = tab.layout.panes().first().cloned();
            }
            tx.tab(tab);
        } else if let Some(mut tab) = c.tab(&p.tab).cloned() {
            match layout::remove(&tab.layout, pane_id) {
                Some(l) => {
                    if tab.focused_pane.as_deref() == Some(pane_id) {
                        tab.focused_pane = l.panes().first().cloned();
                    }
                    if tab.zoomed_pane.as_deref() == Some(pane_id) {
                        tab.zoomed_pane = None;
                    }
                    tab.layout = l;
                    tx.tab(tab);
                }
                None => {
                    orphan_floats = tab.floating.iter().map(|f| f.pane.clone()).collect();
                    tx.event(
                        "tab.closed",
                        json!({"tab": tab.id, "workspace": tab.workspace}),
                        json!({}),
                    );
                    tx.close_tab(&tab);
                    let others = c
                        .tabs_of(&tab.workspace)
                        .iter()
                        .filter(|t| t.id != tab.id)
                        .count();
                    if others == 0
                        && let Some(ws) = c.ws(&tab.workspace).cloned()
                    {
                        tx.event("workspace.closed", json!({"workspace": ws.id}), json!({}));
                        tx.close_ws(&ws);
                    }
                }
            }
        }
        self.tokens.lock().unwrap().retain(|_, p| p != pane_id);
        self.persist_tokens(&mut tx);
        let _ = self.commit(&mut c, tx);
        drop(c);
        for f in orphan_floats {
            self.close_pane(&f);
        }
        self.fix_client_focus();
    }

    /// After panes/tabs disappear, move client focus to something that still exists.
    pub fn fix_client_focus(&self) {
        let (panes, tabs, wss) = self.with_core(|c| {
            (
                c.model
                    .panes
                    .iter()
                    .map(|p| (p.id.clone(), p.tab.clone(), p.workspace.clone()))
                    .collect::<Vec<_>>(),
                c.model
                    .tabs
                    .iter()
                    .map(|t| (t.id.clone(), t.workspace.clone(), t.focused_pane.clone()))
                    .collect::<Vec<_>>(),
                c.model
                    .workspaces
                    .iter()
                    .map(|w| w.id.clone())
                    .collect::<Vec<_>>(),
            )
        });
        let mut clients = self.clients.lock().unwrap();
        for st in clients.values_mut() {
            let f = &mut st.focus;
            if f.pane
                .as_ref()
                .is_some_and(|p| panes.iter().any(|(id, _, _)| id == p))
            {
                continue;
            }
            // Prefer the same tab, then the same workspace, then anything.
            let pick = tabs
                .iter()
                .find(|(id, _, _)| Some(id) == f.tab.as_ref())
                .or_else(|| {
                    tabs.iter()
                        .find(|(_, ws, _)| Some(ws) == f.workspace.as_ref())
                })
                .or_else(|| {
                    wss.first()
                        .and_then(|w| tabs.iter().find(|(_, ws, _)| ws == w))
                });
            match pick {
                Some((tid, wid, fp)) => {
                    f.tab = Some(tid.clone());
                    f.workspace = Some(wid.clone());
                    f.pane = fp.clone().or_else(|| {
                        panes
                            .iter()
                            .find(|(_, t, _)| t == tid)
                            .map(|(p, _, _)| p.clone())
                    });
                }
                None => *f = ClientFocus::default(),
            }
        }
        drop(clients);
        self.bump_model();
    }

    // ---- focus ----------------------------------------------------------------------------

    pub fn focus_pane(&self, client: &str, pane_id: &str) {
        let Some((p, ws)) =
            self.with_core(|c| c.pane(pane_id).cloned().map(|p| (p.clone(), p.workspace)))
        else {
            return;
        };
        let prev = {
            let mut clients = self.clients.lock().unwrap();
            let st = clients.entry(client.to_string()).or_default();
            let prev = st.focus.pane.clone();
            st.focus = ClientFocus {
                workspace: Some(ws),
                tab: Some(p.tab.clone()),
                pane: Some(p.id.clone()),
            };
            st.last_active = Some(Instant::now());
            prev
        };
        let mut c = self.core.lock().unwrap();
        let mut tx = Tx::new();
        if let Some(mut tab) = c.tab(&p.tab).cloned()
            && tab.focused_pane.as_deref() != Some(&p.id)
        {
            tab.focused_pane = Some(p.id.clone());
            tx.tab(tab);
        }
        let mut np = p.clone();
        if np.unread || np.marked_unread {
            np.unread = false;
            np.marked_unread = false;
            tx.pane(np);
        }
        // Mark seen (done → idle) for the focused pane's run (08 §2.3).
        if let Some(r) = c.run_for_pane(&p.id) {
            tx.m.read_mark("local", &p.id, r.done_rev);
        }
        tx.event("pane.focused", subject_pane(&p), json!({"client": client}));
        // Focus and unread marks are UI convenience: applied in memory when storage is down.
        tx.ephemeral = true;
        let _ = self.commit(&mut c, tx);
        drop(c);
        if prev.as_deref() != Some(&p.id) {
            self.agents.on_focus(self, &p.id);
        }
        self.bump_model();
    }

    pub fn client_focus(&self, client: &str) -> ClientFocus {
        self.clients
            .lock()
            .unwrap()
            .get(client)
            .map(|s| s.focus.clone())
            .unwrap_or_default()
    }

    /// The focused pane of the most recently active TUI client (`@focused`).
    pub fn focused_pane(&self) -> Option<String> {
        let clients = self.clients.lock().unwrap();
        clients
            .values()
            .filter(|s| s.kind == "tui")
            .max_by_key(|s| s.last_active)
            .and_then(|s| s.focus.pane.clone())
    }

    /// Whether any attached TUI client has this pane focused (gate-mode decision, 04 §7.2).
    pub fn pane_focused_by_any(&self, pane: &str) -> bool {
        self.clients
            .lock()
            .unwrap()
            .values()
            .any(|s| s.kind == "tui" && s.focus.pane.as_deref() == Some(pane))
    }

    pub fn pane_visible(&self, pane: &str) -> bool {
        self.clients
            .lock()
            .unwrap()
            .values()
            .any(|s| s.visible.iter().any(|v| v == pane))
    }

    // ---- callbacks from pane tasks --------------------------------------------------------

    pub fn holder_epoch(&self, pane: &str, epoch: u64) {
        let mut c = self.core.lock().unwrap();
        if let Ok(hs) = c.store.holders()
            && let Some(h) = hs.into_iter().find(|h| h.pane == pane)
        {
            let mut tx = Tx::new();
            tx.m.holder(pane, &h.socket, &h.key, epoch, h.holder_pid, h.child_pid);
            let _ = c.commit(tx);
        }
    }

    pub fn mark_recovered(&self, pane: &str, method: Option<&str>) {
        let mut c = self.core.lock().unwrap();
        if let Some(mut p) = c.pane(pane).cloned() {
            p.recovered = method.map(str::to_string);
            let mut tx = Tx::new();
            if let Some(m) = method {
                tx.event("pane.recovered", subject_pane(&p), json!({"method": m}));
            }
            tx.pane(p);
            let _ = self.commit(&mut c, tx);
        }
    }

    pub fn pane_resized(&self, pane: &str, cols: u16, rows: u16) {
        let mut c = self.core.lock().unwrap();
        if let Some(mut p) = c.pane(pane).cloned()
            && (p.cols, p.rows) != (cols, rows)
        {
            p.cols = cols;
            p.rows = rows;
            let mut tx = Tx::new();
            tx.pane(p);
            let _ = c.commit(tx);
        }
    }

    pub fn pane_status(self: &Arc<Self>, pane: &str, st: &ProcStatus) {
        {
            let mut c = self.core.lock().unwrap();
            if let Some(mut p) = c.pane(pane).cloned()
                && p.fg_cmdline != st.fg_cmdline
            {
                p.fg_cmdline = st.fg_cmdline.clone();
                if p.title.is_none()
                    && let Some(a) = st.fg_cmdline.first()
                {
                    p.auto_title = a
                        .rsplit('/')
                        .next()
                        .unwrap_or(a)
                        .trim_start_matches('-')
                        .to_string();
                }
                let mut tx = Tx::new();
                tx.event(
                    "pane.process_changed",
                    subject_pane(&p),
                    json!({"fg_cmdline": st.fg_cmdline}),
                );
                tx.pane(p);
                let _ = self.commit(&mut c, tx);
            }
        }
        self.agents.on_process(self, pane, st);
    }

    pub fn pane_exited(&self, pane: &str, code: Option<i32>, signal: Option<i32>) {
        let mut c = self.core.lock().unwrap();
        if let Some(mut p) = c.pane(pane).cloned() {
            p.exited = true;
            p.exit_code = code;
            let mut tx = Tx::new();
            tx.event(
                "pane.exited",
                subject_pane(&p),
                json!({"code": code, "signal": signal}),
            );
            tx.pane(p);
            let _ = self.commit(&mut c, tx);
        }
    }

    pub fn pane_output(&self, pane: &str) {
        if self.pane_visible(pane) {
            return;
        }
        let mut c = self.core.lock().unwrap();
        if let Some(p) = c.pane(pane)
            && !p.unread
        {
            let mut p = p.clone();
            p.unread = true;
            let mut tx = Tx::new();
            tx.pane(p);
            tx.ephemeral = true;
            let _ = self.commit(&mut c, tx);
            drop(c);
            self.bump_model();
        }
    }

    pub fn pane_effect(self: &Arc<Self>, pane: &str, e: Effect, replaying: bool) {
        match e {
            Effect::Bell if !replaying => {
                let _ = self.ui.send(UiEvent::Bell { pane: pane.into() });
            }
            Effect::Notify { kind, title, body } if !replaying => {
                let kind = match kind {
                    NotifyKind::Osc9 => "osc9",
                    NotifyKind::Osc99 => "osc99",
                    NotifyKind::Osc777 => "osc777",
                };
                let t = title.unwrap_or_else(|| {
                    self.with_core(|c| {
                        c.pane(pane)
                            .map(|p| p.display_title().to_string())
                            .unwrap_or_default()
                    })
                });
                self.notify(kind, Some(pane), &t, &body, "normal");
            }
            Effect::Clipboard { primary, data } if !replaying => {
                let _ = self.ui.send(UiEvent::Clipboard {
                    pane: pane.into(),
                    primary,
                    data,
                });
            }
            Effect::Cwd(cwd) => {
                let mut c = self.core.lock().unwrap();
                if let Some(mut p) = c.pane(pane).cloned() {
                    p.cwd = Some(cwd.clone());
                    let mut tx = Tx::new();
                    tx.event("pane.cwd_changed", subject_pane(&p), json!({"cwd": cwd}));
                    tx.pane(p);
                    let _ = self.commit(&mut c, tx);
                }
            }
            Effect::TitleChanged => self.bump_model(),
            // Terminal effects (03 §8); none of them are replayed after a restart.
            Effect::Mark { kind, exit } if !replaying => term_effects::mark(self, pane, kind, exit),
            Effect::Progress { state, pct } if !replaying => {
                term_effects::progress(self, pane, state, pct)
            }
            Effect::UserVar { name, value } if !replaying => {
                term_effects::user_var(self, pane, name, value)
            }
            Effect::ClipboardQuery { primary } if !replaying => {
                let _ = term_effects::clipboard_query(self, pane, primary);
            }
            _ => {}
        }
    }

    pub fn notify(
        &self,
        kind: &str,
        pane: Option<&str>,
        title: &str,
        body: &str,
        urgency: &str,
    ) -> Notification {
        let mut c = self.core.lock().unwrap();
        let n = c.notify(kind, pane, title, body, urgency);
        let mut tx = Tx::new();
        tx.event(
            "notification.created",
            json!({"pane": pane}),
            json!({"id": n.id, "kind": kind, "title": title, "body": body, "urgency": urgency}),
        );
        let _ = self.commit(&mut c, tx);
        drop(c);
        // Pipeline: rules, presence, quiet hours, coalescing, native delivery (08 §7.1).
        let n = notify::deliver(self, n);
        let _ = self.ui.send(UiEvent::Notify(n.clone()));
        n
    }

    /// Persist a VT snapshot taken at holder ring `offset` of holder `incarnation`.
    pub fn store_snapshot(
        &self,
        pane: &str,
        offset: u64,
        blob: Vec<u8>,
        incarnation: &str,
    ) -> bool {
        // Degraded: VT snapshots pause (the recovery guarantee is "ring only", 02 §4a).
        if self.degraded.lock().unwrap().is_some() {
            return false;
        }
        let mut c = self.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.m.snapshot(
            pane,
            offset,
            vk_term::engine::ENGINE,
            vk_term::engine::ENGINE_VERSION,
            blob,
            incarnation,
        );
        c.commit(tx).is_ok()
    }

    pub fn archive_rows(&self, pane: &str, rows: Vec<ArchivedRow>) {
        // A browser console split is `no_archive` (06 B3.2): nothing on disk, nothing indexed.
        if crate::browser_pane::page_io::no_archive(self, pane) {
            return;
        }
        // Degraded: archive writes pause with the snapshots (02 §4a); the rows stay in the ring.
        if self.degraded.lock().unwrap().is_some() {
            self.hardening.archive_skipped(rows.len());
            return;
        }
        let ts = now_ms();
        {
            // Under the archive lock (see `fts_buf`): a purge sees the rows either in both the
            // archive and the buffer, or in neither.
            let mut a = self.archive.lock().unwrap();
            self.fts_buf.lock().unwrap().extend(
                rows.iter()
                    .filter(|r| !r.t.is_empty())
                    .map(|r| (pane.to_string(), r.n, ts, r.t.clone())),
            );
            let _ = a.append(pane, &rows);
        }
        self.housekeeping_wake.notify_one();
    }

    pub fn archive_last_line(&self, pane: &str) -> Option<u64> {
        self.archive.lock().unwrap().last_line(pane).ok().flatten()
    }

    /// Housekeeping: flush archive + FTS, probe storage when degraded. Runs at most once a
    /// second while there is something to do ([`Server::housekeeping_wake`]), never when idle.
    pub fn housekeeping(&self) {
        self.housekeeping_runs.fetch_add(1, Ordering::Relaxed);
        // The archive lock is held from the drain until the batch is indexed: `forget` and
        // retention purge under it, so a purge can't run between the two and have this batch
        // re-insert text it just deleted (leftovers review finding 7).
        let mut a = self.archive.lock().unwrap();
        let _ = a.flush();
        let rows = std::mem::take(&mut *self.fts_buf.lock().unwrap());
        #[cfg(test)]
        if let Some(hook) = self.after_fts_drain.lock().unwrap().as_ref() {
            hook();
        }
        if !rows.is_empty() {
            let c = self.core.lock().unwrap();
            let rows = crate::privacy::index_rows(self, rows);
            let _ = c.store.fts_insert(&rows);
            // Remember each archived pane's workspace for scoped archive search (09 §5.1).
            let mut seen: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
            seen.sort_unstable();
            seen.dedup();
            let ids: Vec<vk_store::ArchivePane> = seen
                .iter()
                .filter_map(|id| c.pane(id))
                .map(|p| {
                    (
                        p.id.clone(),
                        p.workspace.clone(),
                        p.tab.clone(),
                        p.handle.clone(),
                        p.display_title().to_string(),
                    )
                })
                .collect();
            let _ = c.store.fts_register_panes(&ids);
        }
        drop(a);
        if self.degraded.lock().unwrap().is_some() && self.hardening.probe_due() {
            let ok = self.with_core(|c| c.store.probe().is_ok());
            if ok {
                *self.degraded.lock().unwrap() = None;
                self.with_core(|c| c.model.degraded = None);
                self.hardening.recovered();
                self.bump_model();
            }
        }
    }

    /// Scrollback retention (`terminal.archive_max_per_pane`, `terminal.archive_days`): delete
    /// the oldest closed segments of every pane together with their `scrollback_fts` rows and
    /// the `archive_panes` rows of panes left with nothing. The segment being written is never
    /// touched. `0` disables a limit. Runs from the hourly housekeeping pass.
    pub fn archive_retention(&self) {
        let (max_bytes, days) = vk_config::Config::load(vk_config::config_path())
            .map(|(c, _)| (c.terminal.archive_max_per_pane.0, c.terminal.archive_days))
            .unwrap_or((200 << 20, 30));
        self.archive_retention_with(max_bytes, days);
    }

    pub fn archive_retention_with(&self, max_bytes: u64, days: u32) {
        use vk_store::archive::Select;
        let mut a = self.archive.lock().unwrap();
        let _ = a.flush();
        let c = self.core.lock().unwrap();
        if max_bytes > 0 {
            let _ = c
                .store
                .purge_archive(&mut a, None, Select::OverBytes(max_bytes), false);
        }
        if days > 0 {
            let cutoff = now_ms() - i64::from(days) * 86_400_000;
            let _ = c
                .store
                .purge_archive(&mut a, None, Select::OlderThan(cutoff), false);
        }
    }

    pub fn snapshot_json(&self) -> Value {
        let c = self.core.lock().unwrap();
        json!({
            "at_seq": c.store.last_seq().unwrap_or(0),
            "session": c.model.session,
            "machine": c.model.machine,
            "workspaces": c.model.workspaces,
            "tabs": c.model.tabs,
            "panes": c.model.panes,
            "runs": c.model.runs,
            "interactions": c.model.interactions,
            "tasks": c.model.tasks,
            "previews": c.model.previews,
            "groups": c.model.groups,
            "appearance": c.model.appearance,
        })
    }
}
