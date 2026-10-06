//! Machine and Session entities (02 §1.1; 3D): `session.info`, `machine.list|get|upsert|remove`
//! and the `session.started/stopped` and `machine.*` events.
//!
//! This server always knows its own machine (id = the machine uuid, kind `local`, refreshed at
//! start). Remote federation is client-owned: a client that connects to a
//! remote machine reports it with `machine.upsert {label, kind, address, status, ...}` and
//! reports status changes the same way; the server persists the record and emits
//! `machine.added`, `machine.connected`, `machine.disconnected`, `machine.degraded {reason}` and
//! `machine.removed`, so every other client and the audit log see one history.

use crate::Server;
use crate::api::{Ctx, R, err, internal, invalid, not_found, req, s};
use crate::core::Tx;
use serde_json::{Value, json};
use vk_proto::entities::{Machine, MachineKind, MachineStatus, SessionInfo};
use vk_proto::rpc::ErrorKind;
use vk_store::now_ms;

pub const METHODS: &[(&str, bool)] = &[
    ("session.info", false),
    ("machine.list", false),
    ("machine.get", false),
    ("machine.upsert", true),
    ("machine.remove", true),
];

/// Refused to pane tokens: machines are the user's connection list.
pub const PANE_FORBIDDEN: &[&str] = &["machine.upsert", "machine.remove"];

const K_MACHINE: &str = "machine";
const K_SESSION: &str = "session_info";

fn local_machine(server: &Server) -> Machine {
    let id = server.with_core(|c| c.store.machine_uuid.clone());
    Machine {
        id,
        label: server.opts.machine.clone(),
        kind: MachineKind::Local,
        address: None,
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        vibeke_version: vk_proto::VERSION.into(),
        status: MachineStatus::Connected,
        last_seen_ms: now_ms(),
    }
}

pub fn session_info(server: &Server) -> SessionInfo {
    server.with_core(|c| {
        let prev: Option<SessionInfo> =
            c.store.get(K_SESSION, &c.store.session_uuid).ok().flatten();
        SessionInfo {
            id: c.store.session_uuid.clone(),
            name: server.opts.session.clone(),
            machine_id: c.store.machine_uuid.clone(),
            created_at_ms: prev.map(|p| p.created_at_ms).unwrap_or_else(now_ms),
            server_pid: std::process::id(),
            server_version: vk_proto::VERSION.into(),
        }
    })
}

/// Server start: persist the Session and local Machine entities and emit `session.started`.
pub fn start(server: &std::sync::Arc<Server>) {
    let prev: Option<SessionInfo> = server.with_core(|c| {
        c.store
            .get(K_SESSION, &c.store.session_uuid.clone())
            .ok()
            .flatten()
    });
    let info = session_info(server);
    let prev_machine: Option<Machine> = server.with_core(|c| {
        c.store
            .get(K_MACHINE, &c.store.machine_uuid.clone())
            .ok()
            .flatten()
    });
    let me = local_machine(server);
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.m.put(K_SESSION, &info.id, Some(&info.name), &info);
    tx.m.put(K_MACHINE, &me.id, Some(&me.label), &me);
    if prev_machine.is_none() {
        tx.event(
            "machine.added",
            json!({"machine": me.id}),
            json!({"label": me.label, "kind": "local", "address": null}),
        );
    }
    tx.event(
        "session.started",
        json!({}),
        json!({
            "pid": info.server_pid,
            "version": info.server_version,
            "machine": me.id,
            "name": info.name,
            "prev_pid": prev.as_ref().map(|p| p.server_pid),
            "fresh": prev.is_none(),
        }),
    );
    let _ = server.commit(&mut c, tx);
}

/// Graceful stop: emit `session.stopped {reason}` (best effort; a crash leaves no such event,
/// and the next `session.started` carries `prev_pid`).
pub fn stopped(server: &Server, reason: &str) {
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.event(
        "session.stopped",
        json!({}),
        json!({"pid": std::process::id(), "reason": reason}),
    );
    let _ = server.commit(&mut c, tx);
}

fn all(server: &Server) -> Vec<Machine> {
    let mut v: Vec<Machine> = server.with_core(|c| c.store.load(K_MACHINE).unwrap_or_default());
    // The local machine's liveness is this process.
    let local = server.with_core(|c| c.store.machine_uuid.clone());
    for m in v.iter_mut().filter(|m| m.id == local) {
        m.last_seen_ms = now_ms();
        m.vibeke_version = vk_proto::VERSION.into();
    }
    v.sort_by(|a, b| {
        (a.kind != MachineKind::Local, &a.label).cmp(&(b.kind != MachineKind::Local, &b.label))
    });
    v
}

fn find(server: &Server, t: &str) -> Option<Machine> {
    all(server).into_iter().find(|m| m.id == t || m.label == t)
}

/// Id for a remote that did not report its machine uuid: stable per label.
pub fn remote_id(label: &str) -> String {
    format!("m-{}", &blake3::hash(label.as_bytes()).to_hex()[..16])
}

fn valid_label(l: &str) -> bool {
    !l.is_empty()
        && l.len() <= 64
        && l.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@'))
}

fn upsert(server: &Server, p: &Value) -> R {
    let label = req(p, "label")?;
    if !valid_label(label) {
        return Err(invalid(
            "label must be 1-64 characters of letters, digits, `-`, `_`, `.` or `@`",
        ));
    }
    let kind = match s(p, "kind") {
        None => MachineKind::Ssh,
        Some(k) => MachineKind::parse(k)
            .filter(|k| *k != MachineKind::Local)
            .ok_or_else(|| {
                invalid("kind must be ssh or quic (this machine is registered by the server)")
            })?,
    };
    let status =
        match s(p, "status") {
            None => None,
            Some(x) => Some(MachineStatus::parse(x).ok_or_else(|| {
                invalid("status must be connected, connecting, degraded or offline")
            })?),
        };
    let local = server.with_core(|c| c.store.machine_uuid.clone());
    let id = s(p, "id")
        .map(str::to_string)
        .unwrap_or_else(|| remote_id(label));
    if id == local || label == server.opts.machine {
        return Err(err(
            ErrorKind::Conflict,
            "that is this machine; the server registers it itself",
        ));
    }
    let prev = all(server)
        .into_iter()
        .find(|m| m.id == id || m.label == label);
    let id = prev.as_ref().map(|m| m.id.clone()).unwrap_or(id);
    let now = now_ms();
    let mut m = prev.clone().unwrap_or(Machine {
        id: id.clone(),
        label: label.to_string(),
        kind,
        address: None,
        os: String::new(),
        arch: String::new(),
        vibeke_version: String::new(),
        status: MachineStatus::Offline,
        last_seen_ms: 0,
    });
    m.label = label.to_string();
    m.kind = kind;
    if let Some(a) = s(p, "address") {
        m.address = Some(a.to_string());
    }
    for (k, slot) in [
        ("os", &mut m.os),
        ("arch", &mut m.arch),
        ("vibeke_version", &mut m.vibeke_version),
    ] {
        if let Some(v) = s(p, k) {
            *slot = v.to_string();
        }
    }
    let before = prev.as_ref().map(|m| m.status);
    if let Some(st) = status {
        m.status = st;
    }
    if matches!(m.status, MachineStatus::Connected | MachineStatus::Degraded) {
        m.last_seen_ms = now;
    }
    let reason = s(p, "reason").map(|r| r.chars().take(200).collect::<String>());
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    let subject = json!({"machine": m.id});
    if prev.is_none() {
        tx.event(
            "machine.added",
            subject.clone(),
            json!({"label": m.label, "kind": m.kind, "address": m.address}),
        );
    }
    if status.is_some() && before != Some(m.status) {
        match m.status {
            MachineStatus::Connected => {
                tx.event(
                    "machine.connected",
                    subject.clone(),
                    json!({"label": m.label}),
                );
            }
            MachineStatus::Offline => {
                tx.event(
                    "machine.disconnected",
                    subject.clone(),
                    json!({"label": m.label, "reason": reason}),
                );
            }
            MachineStatus::Degraded => {
                tx.event(
                    "machine.degraded",
                    subject.clone(),
                    json!({"label": m.label, "reason": reason}),
                );
            }
            MachineStatus::Connecting => {}
        }
    }
    tx.m.put(K_MACHINE, &m.id, Some(&m.label), &m);
    let events = server.commit(&mut c, tx).map_err(internal)?;
    drop(c);
    Ok(json!({
        "machine": m,
        "created": prev.is_none(),
        "cursor": crate::api::cursor(server, events.last().map(|e| e.seq)),
    }))
}

fn remove(server: &Server, p: &Value) -> R {
    let t = req(p, "machine")?;
    let m = find(server, t).ok_or_else(|| not_found("machine", t))?;
    if m.kind == MachineKind::Local {
        return Err(err(ErrorKind::Conflict, "this machine cannot be removed"));
    }
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.event(
        "machine.removed",
        json!({"machine": m.id}),
        json!({"label": m.label}),
    );
    tx.m.delete(K_MACHINE, &m.id);
    let events = server.commit(&mut c, tx).map_err(internal)?;
    drop(c);
    Ok(json!({"removed": m.id, "cursor": crate::api::cursor(server, events.last().map(|e| e.seq))}))
}

/// Dispatch hook for `session.info` and `machine.*`.
pub fn api(server: &Server, _ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    Some(match method {
        "session.info" => {
            let info = session_info(server);
            let machine = find(server, &info.machine_id);
            let cursor = server.with_core(|c| c.store.cursor(c.store.last_seq().unwrap_or(0)));
            Ok(json!({"session": info, "machine": machine, "cursor": cursor}))
        }
        "machine.list" => {
            let local = server.with_core(|c| c.store.machine_uuid.clone());
            Ok(json!({"machines": all(server), "local": local}))
        }
        "machine.get" => req(p, "machine").and_then(|t| {
            find(server, t)
                .map(|m| json!({"machine": m}))
                .ok_or_else(|| not_found("machine", t))
        }),
        "machine.upsert" => upsert(server, p),
        "machine.remove" => remove(server, p),
        _ => return None,
    })
}

#[cfg(test)]
#[path = "machines_tests.rs"]
mod tests;
