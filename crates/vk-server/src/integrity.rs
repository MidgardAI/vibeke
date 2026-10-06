//! Integration tamper detection (09 §5.3). A malicious agent can disable its own harness hook
//! (that is the harness's permission model, not an escalation through Vibeke), but Vibeke
//! notices:
//!
//! - `vibeke integration install` records a fingerprint of what it installed in
//!   `<state root>/integrations.json` (0600): for hook harnesses (Claude, Codex, Gemini) the
//!   Vibeke-owned hook entries (event, command, version, trust) — other settings in the file
//!   may change freely — and for extension harnesses (pi, omp, OpenCode) the whole managed
//!   file. Uninstall forgets it. A harness installed before this existed is recorded the first
//!   time an agent of it starts (`first_seen`).
//! - When an agent starts, its harness's installed integration is checked against the record
//!   (`integration.doctor`); a mismatch or a removed integration is reported.
//! - While runs of a harness are live, its config file is re-checked every 10 s
//!   (`VIBEKE_INTEGRITY_POLL_MS`; a cheap stat first, the hash only when it changed); a change
//!   is reported once per change. No polling happens without live runs.
//!
//! A report is an `integration_tampered` notification (urgency high), an `integration.tampered`
//! event and an audit record.

use crate::Server;
use crate::api::{Ctx, R, s};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use vk_agents::{Dirs, Harness, InstallState};
use vk_store::now_ms;

#[derive(Default)]
pub struct State {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// harness id → fingerprint last seen while its runs are live (`None` = not installed).
    live: HashMap<String, Option<String>>,
    /// harness id → (mtime, len) of its config file at the last check.
    stat: HashMap<String, Option<(SystemTime, u64)>>,
}

fn is_extension(h: Harness) -> bool {
    matches!(h, Harness::Pi | Harness::Omp | Harness::OpenCode)
}

/// The record file: `<state root>/integrations.json`.
pub fn record_path() -> PathBuf {
    crate::paths::state_root().join("integrations.json")
}

/// Fingerprint of the Vibeke-installed part of `h`'s integration; `None` when not installed.
pub fn fingerprint(h: Harness, dirs: &Dirs) -> Option<String> {
    if is_extension(h) {
        let bytes = std::fs::read(dirs.config_file(h)).ok()?;
        return Some(blake3::hash(&bytes).to_hex().to_string());
    }
    let st = vk_agents::status(h, dirs);
    if st.state == InstallState::NotInstalled {
        return None;
    }
    let hooks: Vec<Value> = st
        .hooks
        .iter()
        .map(|k| {
            json!([
                k.event,
                k.command,
                k.version,
                k.trust.map(|t| format!("{t:?}"))
            ])
        })
        .collect();
    let v = json!({"state": format!("{:?}", st.state), "hooks": hooks});
    Some(blake3::hash(v.to_string().as_bytes()).to_hex().to_string())
}

fn read_records() -> serde_json::Map<String, Value> {
    std::fs::read_to_string(record_path())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

fn write_records(m: &serde_json::Map<String, Value>) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let path = record_path();
    if let Some(d) = path.parent() {
        crate::paths::ensure_private_dir(d)?;
    }
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&tmp)?;
    f.write_all(serde_json::to_string_pretty(&Value::Object(m.clone()))?.as_bytes())?;
    drop(f);
    std::fs::rename(&tmp, &path)
}

fn put_record(h: Harness, dirs: &Dirs, fp: &str, source: &str) -> std::io::Result<()> {
    let mut m = read_records();
    m.insert(
        h.id().into(),
        json!({"fingerprint": fp, "file": dirs.config_file(h), "recorded_at_ms": now_ms(), "source": source}),
    );
    write_records(&m)
}

/// After `vibeke integration install`: record what is now installed.
pub fn record_install(h: Harness, dirs: &Dirs) -> std::io::Result<()> {
    match fingerprint(h, dirs) {
        Some(fp) => put_record(h, dirs, &fp, "install"),
        None => forget(h),
    }
}

/// After `vibeke integration uninstall`: nothing is expected any more.
pub fn forget(h: Harness) -> std::io::Result<()> {
    let mut m = read_records();
    if m.remove(h.id()).is_some() {
        write_records(&m)?;
    }
    Ok(())
}

/// One harness's integrity status.
#[derive(Clone, Debug)]
pub struct Check {
    pub harness: Harness,
    pub file: PathBuf,
    pub fingerprint: Option<String>,
    pub recorded: Option<String>,
    /// `ok` | `changed` (differs from the record) | `removed` (recorded but gone) |
    /// `unrecorded` (installed, no record) | `not_installed`.
    pub status: &'static str,
}

impl Check {
    pub fn tampered(&self) -> bool {
        matches!(self.status, "changed" | "removed")
    }
    pub fn to_json(&self) -> Value {
        let detail = match self.status {
            "ok" => "installed integration matches what `vibeke integration install` wrote",
            "changed" => {
                "the installed integration differs from what `vibeke integration install` wrote; review the hook config, then reinstall"
            }
            "removed" => "the integration was removed outside `vibeke integration uninstall`",
            "unrecorded" => {
                "installed before tamper records existed; recorded on the next agent start"
            }
            _ => "not installed",
        };
        json!({"name": format!("{}.integrity", self.harness.id()), "harness": self.harness.id(),
               "ok": !self.tampered(), "status": self.status, "detail": detail,
               "file": self.file, "fingerprint": self.fingerprint, "recorded": self.recorded})
    }
}

pub fn check(h: Harness, dirs: &Dirs) -> Check {
    let fp = fingerprint(h, dirs);
    let recorded = read_records()
        .get(h.id())
        .and_then(|r| r["fingerprint"].as_str().map(str::to_string));
    let status = match (&fp, &recorded) {
        (Some(a), Some(b)) if a == b => "ok",
        (Some(_), Some(_)) => "changed",
        (None, Some(_)) => "removed",
        (Some(_), None) => "unrecorded",
        (None, None) => "not_installed",
    };
    Check {
        harness: h,
        file: dirs.config_file(h),
        fingerprint: fp,
        recorded,
        status,
    }
}

fn report(server: &Server, h: Harness, reason: &str, file: &std::path::Path, run: Option<&str>) {
    let title = format!("{} integration changed", h.id());
    let body = match reason {
        "changed_during_run" => format!(
            "{} changed while an agent was running. Its hooks may be disabled; check it and run `vibeke integration install {}`.",
            file.display(),
            h.id()
        ),
        "removed" => format!(
            "The Vibeke integration in {} was removed outside `vibeke integration uninstall`.",
            file.display()
        ),
        _ => format!(
            "{} differs from what `vibeke integration install {}` wrote.",
            file.display(),
            h.id()
        ),
    };
    let data = json!({"harness": h.id(), "reason": reason, "file": file});
    {
        let mut c = server.core.lock().unwrap();
        let mut tx = crate::core::Tx::new();
        tx.event("integration.tampered", json!({"run": run}), data.clone());
        let _ = server.commit(&mut c, tx);
    }
    crate::audit::record(
        server,
        "integration.tampered",
        json!({"kind": "system"}),
        json!({"harness": h.id(), "run": run}),
        data,
    );
    server.notify("integration_tampered", None, &title, &body, "high");
}

fn file_stat(p: &std::path::Path) -> Option<(SystemTime, u64)> {
    let m = std::fs::metadata(p).ok()?;
    Some((m.modified().ok()?, m.len()))
}

/// An agent of harness `h` started (run `run`): check against the record (09 §5.3) and take
/// the live baseline.
pub fn on_agent_start(server: &Server, h: Harness, run: &str, dirs: &Dirs) {
    let c = check(h, dirs);
    match c.status {
        "changed" | "removed" => report(server, h, c.status, &c.file, Some(run)),
        "unrecorded" => {
            if let Some(fp) = &c.fingerprint {
                let _ = put_record(h, dirs, fp, "first_seen");
            }
        }
        _ => {}
    }
    let mut g = server.security.integrity.inner.lock().unwrap();
    g.live.insert(h.id().into(), c.fingerprint.clone());
    g.stat.insert(h.id().into(), file_stat(&c.file));
}

/// Re-check harnesses with live runs; report a change once.
pub fn poll(server: &Server, dirs: &Dirs) {
    let live: Vec<(Harness, String)> = server.with_core(|c| {
        let mut v: Vec<(Harness, String)> = c
            .model
            .runs
            .iter()
            .filter(|r| r.ended_at_ms.is_none())
            .filter_map(|r| Harness::from_id(&r.harness).map(|h| (h, r.id.clone())))
            .collect();
        v.sort_by_key(|(h, _)| h.id());
        v.dedup_by_key(|(h, _)| h.id());
        v
    });
    for (h, run) in live {
        let file = dirs.config_file(h);
        let st = file_stat(&file);
        let (base, known) = {
            let g = server.security.integrity.inner.lock().unwrap();
            (g.live.get(h.id()).cloned(), g.stat.get(h.id()).cloned())
        };
        let Some(base) = base else {
            on_agent_start(server, h, &run, dirs);
            continue;
        };
        if known == Some(st) {
            continue;
        }
        let fp = fingerprint(h, dirs);
        {
            let mut g = server.security.integrity.inner.lock().unwrap();
            g.stat.insert(h.id().into(), st);
            g.live.insert(h.id().into(), fp.clone());
        }
        if fp != base && base.is_some() {
            report(server, h, "changed_during_run", &file, Some(&run));
        }
    }
}

fn poll_interval() -> Duration {
    Duration::from_millis(
        std::env::var("VIBEKE_INTEGRITY_POLL_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10_000)
            .max(50),
    )
}

fn has_live_runs(server: &Server) -> bool {
    server.with_core(|c| {
        c.model
            .runs
            .iter()
            .any(|r| r.ended_at_ms.is_none() && Harness::from_id(&r.harness).is_some())
    })
}

/// Watch `agent.started` events and poll while runs are live.
pub fn start(server: &Arc<Server>) {
    let srv = server.clone();
    let mut rx = server.events.subscribe();
    tokio::spawn(async move {
        let every = poll_interval();
        let mut next = tokio::time::Instant::now() + every;
        loop {
            let live = has_live_runs(&srv);
            if !live {
                next = tokio::time::Instant::now() + every;
            }
            tokio::select! {
                ev = rx.recv() => match ev {
                    Ok(e) if e.kind == "agent.started" => {
                        let run = e.subject.get("run").and_then(Value::as_str).map(str::to_string);
                        let harness = run.as_deref().and_then(|r| {
                            srv.with_core(|c| c.run(r).map(|r| r.harness.clone()))
                        });
                        if let (Some(run), Some(h)) = (run, harness.as_deref().and_then(Harness::from_id)) {
                            let s = srv.clone();
                            let _ = tokio::task::spawn_blocking(move || {
                                on_agent_start(&s, h, &run, &Dirs::from_env());
                            })
                            .await;
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => return,
                },
                _ = tokio::time::sleep_until(next), if live => {
                    next = tokio::time::Instant::now() + every;
                    let s = srv.clone();
                    let _ = tokio::task::spawn_blocking(move || poll(&s, &Dirs::from_env())).await;
                }
            }
        }
    });
}

/// `integration.doctor {harness?}` → `{checks}` (full scope only).
pub fn api(_server: &Server, _ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    if method != "integration.doctor" {
        return None;
    }
    let dirs = Dirs::from_env();
    let hs: Vec<Harness> = match s(p, "harness") {
        Some(id) => match Harness::from_id(id) {
            Some(h) => vec![h],
            None => {
                return Some(Err(crate::api::invalid(format!("unknown harness `{id}`"))));
            }
        },
        None => Harness::ALL.to_vec(),
    };
    let checks: Vec<Value> = hs.into_iter().map(|h| check(h, &dirs).to_json()).collect();
    Some(Ok(json!({"checks": checks})))
}
