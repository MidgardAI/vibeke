//! Config and store hardening (02 §2.3, §3, §4a; 3D): configurable event retention with the row
//! cap, the pre-migration backup surface, and degraded-mode bookkeeping.
//!
//! - **Retention** (`[events]` in `config.toml`): `sync_retention` (default `7d`),
//!   `history_retention` (`365d`) and `max_rows` (`2000000`, `0` = no cap). The hourly sweep
//!   ([`sweep`]) prunes events (aged first, then the oldest `sync` rows over the cap, `history`
//!   only when `sync` is gone), the Turn/Item stream of long-ended runs, and unreferenced old
//!   blobs. `storage.prune` runs the same sweep on demand.
//! - **Backups**: the store copies `state.db` to `<state>/backups/` before it migrates (last 3
//!   kept, `vk_store::backup`); `storage.status` lists them; `vibeke doctor --restore-backup`
//!   restores one offline and rotates `log_epoch`.
//! - **Degraded mode**: a failed commit means the mutation did not happen (`storage_unavailable`).
//!   Focus and unread marks are applied in memory and counted (`ephemeral`); interaction answers
//!   that cannot be recorded are refused before anything is delivered ([`refuse_answer`]); VT
//!   snapshots and archive writes pause; a probe write every 5 s ends the mode.

use crate::Server;
use crate::api::{R, err, internal};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use vk_proto::rpc::{ErrorKind, RpcError};
use vk_store::{PruneReport, Retention};

pub const METHODS: &[(&str, bool)] = &[("storage.status", false), ("storage.prune", true)];

/// Refused to pane tokens: paths, retention and the whole session's storage.
pub const PANE_FORBIDDEN: &[&str] = &["storage.status", "storage.prune"];

/// Recovery probe interval while degraded (02 §4a).
pub const PROBE_EVERY_MS: i64 = 5_000;

#[derive(Default)]
pub struct State {
    last_probe_ms: AtomicI64,
    ephemeral: AtomicU64,
    archive_skipped: AtomicU64,
}

impl State {
    /// A UI-convenience transaction was applied in memory only.
    pub fn note_ephemeral(&self) {
        self.ephemeral.fetch_add(1, Ordering::Relaxed);
    }
    /// Storage accepted a write again: nothing is pending any more (nothing is replayed).
    pub fn recovered(&self) {
        self.ephemeral.store(0, Ordering::Relaxed);
    }
    pub fn ephemeral(&self) -> u64 {
        self.ephemeral.load(Ordering::Relaxed)
    }
    pub fn archive_skipped(&self, rows: usize) {
        self.archive_skipped
            .fetch_add(rows as u64, Ordering::Relaxed);
    }
    pub fn archive_skipped_total(&self) -> u64 {
        self.archive_skipped.load(Ordering::Relaxed)
    }
    /// Whether a recovery probe is due (every [`PROBE_EVERY_MS`]); claims the slot when it is.
    pub fn probe_due(&self) -> bool {
        let now = vk_store::now_ms();
        let last = self.last_probe_ms.load(Ordering::Relaxed);
        if now - last >= PROBE_EVERY_MS {
            self.last_probe_ms.store(now, Ordering::Relaxed);
            true
        } else {
            false
        }
    }
}

/// `[events]` settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventsConfig {
    pub retention: Retention,
    /// Unreferenced uploads and payloads older than this many days are collected.
    pub blob_days: i64,
}

impl Default for EventsConfig {
    fn default() -> Self {
        EventsConfig {
            retention: Retention::default(),
            blob_days: crate::blob_store::DEFAULT_GC_DAYS,
        }
    }
}

impl EventsConfig {
    /// Read `[events]`; an absent, malformed or non-positive value keeps its default (the load
    /// warnings are `vk-config`'s).
    pub fn from_config(cfg: &vk_config::Config) -> Self {
        let e = cfg.events();
        EventsConfig {
            retention: Retention {
                sync_days: e.sync_days,
                history_days: e.history_days,
                max_rows: e.max_rows,
            },
            blob_days: e.blob_days,
        }
    }

    pub fn current() -> Self {
        Self::from_config(&crate::config_api::current())
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SweepReport {
    pub events: PruneReport,
    pub stream_removed: usize,
    pub blobs_removed: usize,
    pub blob_bytes: u64,
}

/// The retention sweep (hourly, and `storage.prune`). Does nothing while storage is degraded.
pub fn sweep(server: &Server) -> SweepReport {
    sweep_with(server, &EventsConfig::current())
}

pub fn sweep_with(server: &Server, cfg: &EventsConfig) -> SweepReport {
    let mut r = SweepReport::default();
    if server.degraded.lock().unwrap().is_some() {
        return r;
    }
    match server.with_core(|c| c.store.prune_with(&cfg.retention)) {
        Ok(p) => r.events = p,
        Err(e) => tracing::warn!(error = %e, "event retention sweep failed"),
    }
    let cutoff = vk_store::now_ms() - cfg.retention.history_days * 86_400_000;
    r.stream_removed = crate::items::prune(server, cutoff);
    let g = crate::blob_store::gc(server, cfg.blob_days, false);
    r.blobs_removed = g.removed;
    r.blob_bytes = g.bytes;
    if r.events.aged + r.events.capped + r.stream_removed + r.blobs_removed > 0 {
        tracing::info!(
            aged = r.events.aged,
            capped = r.events.capped,
            stream = r.stream_removed,
            blobs = r.blobs_removed,
            "storage sweep"
        );
    }
    r
}

/// The refusal for an interaction answer that cannot be recorded while storage is down: a
/// decision is never delivered unless it was recorded first (02 §4a).
pub fn refuse_answer(server: &Server) -> Result<(), RpcError> {
    match server.degraded.lock().unwrap().as_deref() {
        None => Ok(()),
        Some(reason) => Err(answer_refusal(reason)),
    }
}

pub fn answer_refusal(reason: &str) -> RpcError {
    err(
        ErrorKind::StorageUnavailable,
        "the decision could not be recorded (storage is unavailable), so it was not delivered: answer in the agent's own UI",
    )
    .details(json!({"degraded": reason, "fallback": "answer in the agent's own UI"}))
}

fn status(server: &Server) -> R {
    let degraded = server.degraded.lock().unwrap().clone();
    let cfg = EventsConfig::current();
    let db = server.paths.db();
    let (events, first, last, bytes, epoch, session, machine) = server.with_core(|c| {
        (
            c.store.event_count().unwrap_or(0),
            c.store.earliest_seq().unwrap_or(0),
            c.store.last_seq().unwrap_or(0),
            c.store.db_bytes().unwrap_or(0),
            c.store.log_epoch.clone(),
            c.store.session_uuid.clone(),
            c.store.machine_uuid.clone(),
        )
    });
    let backups: Vec<Value> = vk_store::backup::list_backups(&db)
        .into_iter()
        .map(|b| {
            json!({"name": b.name, "schema_version": b.schema_version, "created_at": b.created_at_ms, "bytes": b.bytes})
        })
        .collect();
    let blobs = crate::blob_store::store(server).stats();
    Ok(json!({
        "degraded": degraded,
        "ephemeral": server.hardening.ephemeral(),
        "archive_rows_skipped": server.hardening.archive_skipped_total(),
        "db": {"path": db, "bytes": bytes},
        "events": {
            "count": events,
            "first_seq": first,
            "last_seq": last,
            "retention": {
                "sync_days": cfg.retention.sync_days,
                "history_days": cfg.retention.history_days,
                "max_rows": cfg.retention.max_rows,
                "blob_days": cfg.blob_days,
            },
        },
        "backups": backups,
        "keep_backups": vk_store::backup::KEEP_BACKUPS,
        "blobs": {"count": blobs.count, "bytes": blobs.bytes},
        "cursor": {"machine_uuid": machine, "session_uuid": session, "log_epoch": epoch},
    }))
}

fn prune(server: &Server) -> R {
    if let Some(reason) = server.degraded.lock().unwrap().clone() {
        return Err(err(
            ErrorKind::StorageUnavailable,
            "storage is unavailable; nothing was pruned",
        )
        .details(json!({"degraded": reason})));
    }
    let r = sweep(server);
    let remaining = server
        .with_core(|c| c.store.event_count())
        .map_err(internal)?;
    Ok(json!({
        "events_aged": r.events.aged,
        "events_capped": r.events.capped,
        "events_remaining": remaining,
        "stream_removed": r.stream_removed,
        "blobs_removed": r.blobs_removed,
        "blob_bytes": r.blob_bytes,
    }))
}

/// Dispatch hook for `storage.status` / `storage.prune`.
pub fn api(server: &Server, method: &str, _p: &Value) -> Option<R> {
    Some(match method {
        "storage.status" => status(server),
        "storage.prune" => prune(server),
        _ => return None,
    })
}

#[cfg(test)]
pub(crate) mod testkit {
    use crate::paths::Paths;
    use crate::{Server, ServerOpts};
    use std::sync::{Arc, Once};

    /// Throwaway server in `dir`; the installation's state and config roots are temp dirs.
    pub fn server(dir: &std::path::Path, session: &str) -> Arc<Server> {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            let base =
                std::env::temp_dir().join(format!("vk-hardening-tests-{}", std::process::id()));
            std::fs::create_dir_all(&base).unwrap();
            // SAFETY: set once before any server of this test binary reads them; never the
            // user's real directories.
            unsafe {
                if std::env::var_os("VIBEKE_STATE_DIR").is_none() {
                    std::env::set_var("VIBEKE_STATE_DIR", base.join("state"));
                }
                if std::env::var_os("VIBEKE_CONFIG").is_none() {
                    std::env::set_var("VIBEKE_CONFIG", base.join("config.toml"));
                }
            }
        });
        let root = dir.canonicalize().unwrap();
        let paths = Paths {
            session: session.into(),
            runtime: root.join("run").join(session),
            state: root.join("state").join(session),
        };
        let opts = ServerOpts {
            session: session.into(),
            machine: "m".into(),
            bin: "/bin/false".into(),
            hold_args: vec![],
            default_shell: None,
            env: vec![],
            shims: false,
        };
        Server::new(paths, opts).unwrap()
    }

    pub fn user() -> crate::api::Ctx {
        crate::api::Ctx {
            client_id: "c-user".into(),
            kind: "cli".into(),
            pane_scope: None,
            remote: false,
        }
    }

    pub fn pane_ctx(p: &str) -> crate::api::Ctx {
        crate::api::Ctx {
            client_id: format!("c-{p}"),
            kind: "cli".into(),
            pane_scope: Some(p.into()),
            remote: false,
        }
    }

    pub fn sample_run(id: &str, pane: &str) -> vk_proto::model::AgentRun {
        use vk_proto::model::*;
        AgentRun {
            id: id.into(),
            handle: id.into(),
            name: None,
            pane: pane.into(),
            harness: "claude".into(),
            harness_version: None,
            integration: "hooks".into(),
            harness_session_id: None,
            transcript_path: None,
            resume_argv: vec![],
            cwd: None,
            model: None,
            task: None,
            execution: Facet {
                value: Execution::Working,
                since_ms: 0,
                source: StateSource::Structured,
                confidence: 1.0,
                detail: None,
            },
            health: AdapterHealth::Healthy,
            yolo: false,
            permission_mode: None,
            last_message: None,
            last_tool: None,
            turns_completed: 0,
            done_rev: 0,
            started_at_ms: 0,
            ended_at_ms: None,
            capabilities: vec![],
            usage: Default::default(),
            rate_limit: None,
        }
    }

    pub fn sample_interaction(id: &str, run: &str, pane: &str) -> vk_proto::model::Interaction {
        use vk_proto::model::*;
        Interaction {
            id: id.into(),
            handle: id.into(),
            run: run.into(),
            pane: pane.into(),
            kind: InteractionKind::Approval,
            status: InteractionStatus::Open,
            title: "Bash".into(),
            body_md: None,
            action: None,
            questions: vec![],
            plan_md: None,
            answer_channel: AnswerChannel::Native,
            native_ref: None,
            source: StateSource::Structured,
            confidence: 1.0,
            answerable: true,
            gate: false,
            decision_rev: 0,
            delivery: DeliveryState::None,
            delivery_error: None,
            answer: None,
            answered_by: None,
            answer_key: None,
            opened_at_ms: 0,
            answered_at_ms: None,
        }
    }

    pub fn sample_pane(id: &str, ws: &str) -> vk_proto::model::Pane {
        vk_proto::model::Pane {
            id: id.into(),
            handle: id.into(),
            tab: "tab".into(),
            workspace: ws.into(),
            title: None,
            auto_title: String::new(),
            cwd: None,
            cols: 80,
            rows: 24,
            child_pid: None,
            fg_cmdline: vec![],
            exited: false,
            exit_code: None,
            unread: false,
            marked_unread: false,
            pinned: false,
            created_by: "user".into(),
            recovered: None,
            isolation: Default::default(),
            browser: None,
        }
    }
}

#[cfg(test)]
#[path = "hardening_tests.rs"]
mod tests;
