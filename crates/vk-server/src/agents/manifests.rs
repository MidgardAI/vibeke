//! Process-wide harness manifest registry (04 §5): built-ins, the verified remote-channel cache,
//! user manifests (`<config dir>/harnesses/*.toml`) and trusted repo manifests (`repo:<id>`).
//!
//! `Harness::Custom(ManifestRef)` indexes a slot here. Slots are append-only so a `ManifestRef`
//! stays valid across reloads; a reload replaces the manifest behind an existing id.

use super::harness::{Harness, ManifestRef};
use crate::Server;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use std::time::{Duration, Instant};
use vk_agents::manifest::{self as m, Loaded, Sources};

pub struct Slot {
    pub id: &'static str,
    pub display: &'static str,
    pub loaded: Arc<Loaded>,
}

#[derive(Default)]
struct Reg {
    slots: Vec<Slot>,
    by_id: HashMap<String, u32>,
    sources: Sources,
    warnings: Vec<String>,
    /// Synthesized `acp:<name>` manifests (kept across reloads).
    synth: HashMap<String, String>,
}

static REG: LazyLock<RwLock<Reg>> = LazyLock::new(|| {
    let mut r = Reg::default();
    apply(&mut r, m::load(&Sources::default()));
    RwLock::new(r)
});

fn leak(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

fn apply(r: &mut Reg, set: m::Set) {
    r.warnings = set.warnings.clone();
    for l in set.manifests {
        let id = l.m.id.clone();
        let display = l.display().to_string();
        match r.by_id.get(&id) {
            Some(&i) => {
                let slot = &mut r.slots[i as usize];
                if slot.display != display {
                    slot.display = leak(&display);
                }
                slot.loaded = Arc::new(l);
            }
            None => {
                r.by_id.insert(id.clone(), r.slots.len() as u32);
                r.slots.push(Slot {
                    id: leak(&id),
                    display: leak(&display),
                    loaded: Arc::new(l),
                });
            }
        }
    }
}

fn reload_locked(r: &mut Reg) {
    let set = m::load(&r.sources);
    apply(r, set);
    let synth: Vec<(String, String)> = r
        .synth
        .iter()
        .map(|(a, b)| (a.clone(), b.clone()))
        .collect();
    for (id, toml) in synth {
        if let Some(l) = build_synth(r, &id, &toml) {
            apply(
                r,
                m::Set {
                    manifests: vec![l],
                    warnings: r.warnings.clone(),
                },
            );
        }
    }
}

/// User manifest dir: next to the config file (`VIBEKE_CONFIG` honoured).
pub fn user_dir() -> PathBuf {
    vk_config::config_path()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("harnesses")
}

/// Server start: load user manifests and the verified remote cache.
pub fn init() {
    let mut r = REG.write().unwrap();
    r.sources.user_dir = Some(user_dir());
    r.sources.remote = super::channel::cached_dir();
    reload_locked(&mut r);
    for w in &r.warnings {
        tracing::warn!("harness manifest: {w}");
    }
}

pub fn reload() -> Vec<String> {
    let mut r = REG.write().unwrap();
    reload_locked(&mut r);
    r.warnings.clone()
}

pub fn warnings() -> Vec<String> {
    REG.read().unwrap().warnings.clone()
}

pub fn get(r: ManifestRef) -> Option<(&'static str, &'static str, Arc<Loaded>)> {
    let g = REG.read().unwrap();
    g.slots
        .get(r.0 as usize)
        .map(|s| (s.id, s.display, s.loaded.clone()))
}

pub fn lookup(id: &str) -> Option<(ManifestRef, Arc<Loaded>)> {
    let g = REG.read().unwrap();
    let i = *g.by_id.get(id)?;
    Some((ManifestRef(i), g.slots[i as usize].loaded.clone()))
}

pub fn all() -> Vec<(ManifestRef, Arc<Loaded>)> {
    let g = REG.read().unwrap();
    g.slots
        .iter()
        .enumerate()
        .map(|(i, s)| (ManifestRef(i as u32), s.loaded.clone()))
        .collect()
}

fn build_synth(r: &Reg, id: &str, toml: &str) -> Option<Loaded> {
    let _ = r;
    let raw = m::parse_raw(toml, m::Source::Builtin).ok()?;
    let mut base: toml::Table = m::BUILTIN
        .iter()
        .find(|(f, _)| *f == "acp.toml")?
        .1
        .parse()
        .ok()?;
    m::deep_merge(&mut base, &raw.table);
    let man: m::Manifest = toml::Value::Table(base).try_into().ok()?;
    Loaded::new(man, m::Source::Builtin, "acp".into())
        .ok()
        .map(|mut l| {
            l.m.id = id.to_string();
            l
        })
}

/// `acp:<name>`: a harness run in ACP mode through `vibeke acp-host` (04 §6.6). `<name>` may be a
/// known manifest (its `[launch] acp_argv`, name and version command are inherited) or any
/// command name.
pub fn synth_acp(id: &str) -> Option<Harness> {
    let name = id.strip_prefix("acp:")?;
    if name.is_empty() || name.len() > 32 || name.contains(['"', '\\', '\n']) {
        return None;
    }
    if let Some((r, _)) = lookup(id) {
        return Some(Harness::Custom(r));
    }
    let base = lookup(name).map(|(_, l)| l);
    let display = base
        .as_ref()
        .map(|l| format!("{} (ACP)", l.display()))
        .unwrap_or_else(|| format!("{name} (ACP)"));
    let mut t = toml::Table::new();
    t.insert("id".into(), toml::Value::String(id.into()));
    t.insert("name".into(), toml::Value::String(display));
    if let Some(l) = &base {
        let mut launch = toml::Table::new();
        launch.insert(
            "acp_argv".into(),
            toml::Value::Array(
                l.m.launch
                    .acp_argv
                    .iter()
                    .map(|a| toml::Value::String(a.clone()))
                    .collect(),
            ),
        );
        t.insert("launch".into(), toml::Value::Table(launch));
    }
    let text = toml::to_string(&t).ok()?;
    let mut g = REG.write().unwrap();
    let l = build_synth(&g, id, &text)?;
    g.synth.insert(id.to_string(), text);
    let warnings = g.warnings.clone();
    apply(
        &mut g,
        m::Set {
            manifests: vec![l],
            warnings,
        },
    );
    let i = *g.by_id.get(id)?;
    Some(Harness::Custom(ManifestRef(i)))
}

// ---- trusted repo manifests (09 §4 rule 4) ---------------------------------------------------

struct RepoCache {
    /// cwd → (checked at, repo root with `.vibeke/harnesses`, if any).
    roots: HashMap<PathBuf, (Instant, Option<PathBuf>)>,
    /// root → (digest, trusted, checked at).
    seen: HashMap<PathBuf, (String, bool, Instant)>,
}

static REPOS: LazyLock<Mutex<RepoCache>> = LazyLock::new(|| {
    Mutex::new(RepoCache {
        roots: HashMap::new(),
        seen: HashMap::new(),
    })
});

fn find_root(cwd: &Path) -> Option<PathBuf> {
    let mut d = Some(cwd);
    let mut n = 0;
    while let Some(p) = d {
        if p.join(".vibeke/harnesses").is_dir() {
            return Some(p.to_path_buf());
        }
        n += 1;
        if n > 24 {
            break;
        }
        d = p.parent();
    }
    None
}

/// Load `<repo>/.vibeke/harnesses/*.toml` as `repo:<id>` when the repo's `.vibeke/` digest is
/// trusted (`vibeke policy trust`). Checked at most every 10 s per cwd (this runs from the 1 Hz
/// process watcher); a changed `.vibeke/` needs re-trust before its manifests load again.
pub fn ensure_repo(server: &Server, cwd: &Path) {
    let root = {
        let mut c = REPOS.lock().unwrap();
        match c.roots.get(cwd) {
            Some((t, r)) if t.elapsed() < Duration::from_secs(10) => r.clone(),
            _ => {
                let r = find_root(cwd);
                c.roots
                    .insert(cwd.to_path_buf(), (Instant::now(), r.clone()));
                r
            }
        }
    };
    let Some(root) = root else { return };
    let Some(digest) = crate::run::vibeke_dir_digest(&root) else {
        return;
    };
    // Unchanged and trusted: nothing to do. Unchanged and untrusted: re-check trust at most
    // every 2 s (`vibeke policy trust` may have run since).
    if REPOS
        .lock()
        .unwrap()
        .seen
        .get(&root)
        .is_some_and(|(d, t, at)| *d == digest && (*t || at.elapsed() < Duration::from_secs(2)))
    {
        return;
    }
    let trusted = crate::run::repo_trusted(server, &root, &digest);
    REPOS
        .lock()
        .unwrap()
        .seen
        .insert(root.clone(), (digest, trusted, Instant::now()));
    let mut r = REG.write().unwrap();
    let had = r.sources.trusted_repos.contains(&root);
    if trusted && !had {
        r.sources.trusted_repos.push(root);
        reload_locked(&mut r);
    } else if !trusted && had {
        // Changed since trust: stop detecting with it (slots stay, detection skips the root).
        r.sources.trusted_repos.retain(|x| x != &root);
        reload_locked(&mut r);
    } else if !trusted {
        tracing::info!(
            "{}: repo harness manifests not loaded (run `vibeke policy trust` after review)",
            root.display()
        );
    }
}

/// A native plugin's `harness` contribution (07 §7.4): `root` is a Vibeke-owned copy laid out
/// like a repository (`<root>/.vibeke/harnesses/*.toml`), loaded with the same restrictions as a
/// trusted repo's manifests (`repo:<id>`); the plugin's consent is the trust decision.
pub fn set_plugin_root(root: &Path, on: bool) {
    let mut r = REG.write().unwrap();
    let had = r.sources.trusted_repos.iter().any(|x| x == root);
    if on && !had {
        r.sources.trusted_repos.push(root.to_path_buf());
        // Loaded with plugin restrictions: nothing that runs commands (vk_agents
        // `sanitize_plugin`).
        r.sources.plugin_roots.push(root.to_path_buf());
    } else if !on && had {
        r.sources.trusted_repos.retain(|x| x != root);
        r.sources.plugin_roots.retain(|x| x != root);
    } else if !on {
        return;
    }
    reload_locked(&mut r);
}

/// `policy.trust` changed: re-check every repo on its next detection.
pub fn forget_repo_trust() {
    let mut c = REPOS.lock().unwrap();
    c.seen.clear();
    c.roots.clear();
}

/// Is this slot's repo still trusted (a reload after distrust leaves the stale slot behind)?
pub fn repo_active(l: &Loaded) -> bool {
    match &l.source {
        m::Source::Repo { root, .. } => REG.read().unwrap().sources.trusted_repos.contains(root),
        _ => true,
    }
}

/// Process detection for a pane (04 §5.2): repo manifests are refreshed for the cwd first.
pub fn detect_pane(server: &Server, pane: &str, pgid: u32) -> Option<(Harness, Vec<String>)> {
    let tree = vk_hold::procinfo::tree(pgid, 6);
    let cwd = server
        .with_core(|c| c.pane(pane).and_then(|p| p.cwd.clone()))
        .or_else(|| tree.first().and_then(|p| p.cwd.clone()));
    if let Some(c) = &cwd {
        ensure_repo(server, Path::new(c));
    }
    let procs: Vec<(Vec<String>, Option<String>)> =
        tree.into_iter().map(|p| (p.argv, p.exe)).collect();
    super::harness::detect_tree(&procs, cwd.as_deref().map(Path::new))
}

#[cfg(test)]
pub(crate) fn test_register(text: &str) -> Harness {
    let raw = m::parse_raw(text, m::Source::User(PathBuf::from("test.toml"))).unwrap();
    let dir = std::env::temp_dir().join(format!("vk-manifest-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{}.toml", raw.id)), text).unwrap();
    let mut r = REG.write().unwrap();
    let mut src = r.sources.clone();
    src.user_dir = Some(dir);
    r.sources = src;
    reload_locked(&mut r);
    let i = *r.by_id.get(&raw.id).unwrap();
    Harness::Custom(ManifestRef(i))
}
