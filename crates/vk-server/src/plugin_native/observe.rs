//! Registry observation, reconciliation and the native dev loop (02 §3 "Plugin state
//! ownership", 07 §7.6).
//!
//! Once a second the server re-reads `plugins.json` when it changed (CLI installs work with no
//! server running; running sessions observe committed generations):
//!
//! * the committed change is diffed into native events — `plugin.installed`, `plugin.linked`,
//!   `plugin.unlinked`, `plugin.uninstalled`, `plugin.enabled`, `plugin.disabled`,
//!   `plugin.trust_changed` — and `plugin.registry_observed {generation}` (Herdr entries
//!   included; their hooks still see only the baseline projection);
//! * native runtime state follows: processes of plugins that are no longer active (or whose
//!   consent changed) are stopped, their tokens revoked and contributions dropped; autostart
//!   processes of newly active plugins are started; clients are told to re-read actions and
//!   bindings (`plugin.registry_changed`).
//!
//! Dev loop: a **linked** active native plugin is hot-restarted when its manifest, the files of
//! its process command or its `[process] watch` globs change (`plugin.reloaded`); argv actions
//! re-read the manifest on every invocation anyway.
//!
//! Opt-in auto-update (`[plugins] auto_update = "patch"`, 09 §6): once a day the server runs
//! `vibeke plugin update --auto`, which applies only same-major.minor, non-widening updates of
//! repository-installed native plugins (never Herdr plugins, whose updates always need review).

use super::{emit, process, state, tokens};
use crate::Server;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use vk_compat::herdr::registry::Registry;
use vk_compat::native::registry::{NativeStatus, native_status};

#[derive(Debug, Clone, PartialEq)]
pub struct Obs {
    pub kind: &'static str,
    pub managed: bool,
    pub enabled: bool,
    pub root: std::path::PathBuf,
    /// Grant id (Herdr) or consent id (native).
    pub trust: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub generation: u64,
    pub plugins: BTreeMap<String, Obs>,
}

pub fn snapshot(reg: &Registry) -> Snapshot {
    let mut plugins = BTreeMap::new();
    for e in reg.plugins.values() {
        plugins.insert(
            e.id.clone(),
            Obs {
                kind: "herdr",
                managed: e.managed,
                root: e.root.clone(),
                enabled: e.enabled,
                trust: e.trust.as_ref().map(|g| g.grant_id.clone()),
            },
        );
    }
    for e in reg.native.values() {
        plugins.insert(
            e.id.clone(),
            Obs {
                kind: "native",
                managed: e.managed,
                root: e.root.clone(),
                enabled: e.enabled,
                trust: e.consent.as_ref().map(|c| c.consent_id.clone()),
            },
        );
    }
    Snapshot {
        generation: reg.generation,
        plugins,
    }
}

/// Native events for a committed change `old` → `new`: `(type, plugin, data)`.
pub fn diff(old: &Snapshot, new: &Snapshot) -> Vec<(&'static str, String, Value)> {
    let mut out = vec![];
    for (id, n) in &new.plugins {
        match old.plugins.get(id) {
            None => {
                let t = if n.managed {
                    "plugin.installed"
                } else {
                    "plugin.linked"
                };
                out.push((t, id.clone(), json!({"kind": n.kind})));
            }
            Some(o) => {
                if o.managed != n.managed || o.root != n.root {
                    // Re-registered (link ↔ install) or updated (a new managed checkout).
                    let t = if n.managed {
                        "plugin.installed"
                    } else {
                        "plugin.linked"
                    };
                    out.push((t, id.clone(), json!({"kind": n.kind, "update": true})));
                }
                if o.enabled != n.enabled {
                    let t = if n.enabled {
                        "plugin.enabled"
                    } else {
                        "plugin.disabled"
                    };
                    out.push((t, id.clone(), json!({"kind": n.kind})));
                }
                if o.trust != n.trust {
                    out.push((
                        "plugin.trust_changed",
                        id.clone(),
                        json!({"kind": n.kind, "trusted": n.trust.is_some()}),
                    ));
                }
            }
        }
    }
    for (id, o) in &old.plugins {
        if !new.plugins.contains_key(id) {
            let t = if o.managed {
                "plugin.uninstalled"
            } else {
                "plugin.unlinked"
            };
            out.push((t, id.clone(), json!({"kind": o.kind})));
        }
    }
    out
}

/// One observation pass: diff, events, reconcile. Returns whether the registry changed.
pub async fn tick(server: &Arc<Server>) -> bool {
    tokens::sweep(server);
    let reg = super::registry(server);
    let snap = snapshot(&reg);
    let old = state(server).observed.lock().unwrap().clone();
    let changed = old.as_ref() != Some(&snap);
    if changed {
        if let Some(old) = &old {
            for (t, id, data) in diff(old, &snap) {
                emit(server, t, json!({"plugin": id}), json!({"kind": "system"}), data);
            }
        }
        emit(
            server,
            "plugin.registry_observed",
            json!({}),
            json!({"kind": "system"}),
            json!({"generation": snap.generation, "plugins": snap.plugins.len()}),
        );
        *state(server).observed.lock().unwrap() = Some(snap);
        if old.is_some() {
            // Clients re-read actions, bindings, views (and the compat views purge).
            let user = super::plugin_ctx("system", "cli");
            let user = crate::api::Ctx {
                kind: "cli".into(),
                ..user
            };
            let _ = crate::compat::api(server, &user, "plugin.registry.notify", &json!({})).await;
        }
    }
    reconcile(server, &reg).await;
    changed
}

/// Stop what must not run, start what should.
async fn reconcile(server: &Arc<Server>, reg: &Registry) {
    let running: Vec<(String, String)> = state(server)
        .procs
        .lock()
        .unwrap()
        .iter()
        .map(|(id, h)| (id.clone(), h.consent_id.clone()))
        .collect();
    for (id, consent) in running {
        let ok = reg.native.get(&id).is_some_and(|e| {
            native_status(e, super::vibeke_version()).0 == NativeStatus::Active
                && e.consent.as_ref().map(|c| c.consent_id.as_str()) == Some(consent.as_str())
        });
        if !ok {
            process::stop(server, &id, "no longer active").await;
            tokens::revoke_plugin(server, &id);
            super::ui::clear(server, &id);
        }
    }
    // Tokens and contributions of plugins that are no longer active.
    let ids: Vec<String> = state(server).ui.lock().unwrap().by_plugin.keys().cloned().collect();
    for id in ids {
        let ok = reg
            .native
            .get(&id)
            .is_some_and(|e| native_status(e, super::vibeke_version()).0 == NativeStatus::Active);
        if !ok {
            super::ui::clear(server, &id);
            tokens::revoke_plugin(server, &id);
        }
    }
    process::ensure(server);
}

/// Fingerprint of what a linked plugin's hot restart watches.
pub fn fingerprint(root: &Path, command: &[String], watch: &[String]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let stamp = |p: &Path, h: &mut std::collections::hash_map::DefaultHasher| {
        if let Ok(m) = std::fs::metadata(p) {
            p.hash(h);
            m.len().hash(h);
            m.modified().ok().hash(h);
        }
    };
    stamp(&root.join(vk_compat::native::MANIFEST_FILE), &mut h);
    for a in command {
        let p = root.join(a);
        if p.is_file() {
            stamp(&p, &mut h);
        }
    }
    if !watch.is_empty() {
        let mut files = vec![];
        walk(root, root, 0, &mut files);
        files.sort();
        for rel in files {
            if watch.iter().any(|g| vk_store::glob_match(g, &rel)) {
                stamp(&root.join(&rel), &mut h);
            }
        }
    }
    h.finish()
}

fn walk(root: &Path, dir: &Path, depth: usize, out: &mut Vec<String>) {
    if depth > 8 || out.len() > 10_000 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name();
        if name == ".git" || name == "node_modules" {
            continue;
        }
        let p = e.path();
        match e.file_type() {
            Ok(t) if t.is_dir() => walk(root, &p, depth + 1, out),
            Ok(t) if t.is_file() => {
                if let Ok(rel) = p.strip_prefix(root) {
                    out.push(rel.to_string_lossy().into_owned());
                }
            }
            _ => {}
        }
    }
}

/// Hot-restart linked plugins whose watched files changed.
pub async fn dev_watch(server: &Arc<Server>) {
    let reg = super::registry(server);
    for (e, m) in reg.native_active(super::vibeke_version()) {
        if e.managed {
            continue;
        }
        let Some(p) = &m.process else { continue };
        let fp = fingerprint(&e.root, &p.command, &p.watch);
        let prev = state(server)
            .fingerprints
            .lock()
            .unwrap()
            .insert(e.id.clone(), fp);
        let running = state(server).procs.lock().unwrap().contains_key(&e.id);
        if prev.is_some_and(|x| x != fp) && running {
            emit(
                server,
                "plugin.reloaded",
                json!({"plugin": e.id}),
                json!({"kind": "system"}),
                json!({"reason": "files changed"}),
            );
            process::stop(server, &e.id, "dev reload").await;
            state(server).crashes.lock().unwrap().remove(&e.id);
            process::start(server, &e.id);
        }
    }
}

/// `[plugins] auto_update = "patch"`: run `vibeke plugin update --auto` once a day.
fn auto_update(server: &Server) {
    if super::setting("", "auto_update").and_then(|v| v.as_str().map(str::to_string))
        != Some("patch".into())
    {
        return;
    }
    let bin = server.opts.bin.clone();
    let session = server.paths.session.clone();
    tokio::spawn(async move {
        let r = tokio::process::Command::new(&bin)
            .args(["--session", &session, "plugin", "update", "--auto", "--json"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .status()
            .await;
        if let Err(e) = r {
            tracing::warn!(error = %e, "plugin auto-update did not run");
        }
    });
}

/// The observer task (started by [`super::start`]).
pub async fn run(server: Arc<Server>) {
    // After restore and API readiness.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut last_update: Option<Instant> = None;
    loop {
        tick(&server).await;
        dev_watch(&server).await;
        if last_update.is_none_or(|t| t.elapsed() > Duration::from_secs(24 * 3600)) {
            if last_update.is_some() {
                auto_update(&server);
            }
            last_update = Some(Instant::now());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}
