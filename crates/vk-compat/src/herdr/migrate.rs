//! Copy-based migration of Herdr plugin registrations, config and state into Vibeke's own
//! plugin directories (07 §7.6, §7.7 "migration is copy-based, reports conflicts and supports
//! rollback to Herdr's untouched data"; 08 §8.1).
//!
//! * The source is **always an explicit directory** given by the user (`--from`). Nothing here
//!   has a default source, so Herdr's live `~/.config/herdr` is only ever read when the user
//!   names it, and even then it is only read: files are copied, never moved, renamed or
//!   rewritten, and symlinks are reported, not followed.
//! * Destinations are Vibeke's per-plugin config/state dirs ([`PluginDirs`]). An existing
//!   destination file with different content is a **conflict** and is left alone; identical
//!   files are skipped. Every file (and directory) the migration creates is recorded in a
//!   migration record, and [`rollback`] removes exactly those that are still unchanged.
//! * Registrations found in the source `plugins.json` are reported with their source paths.
//!   Linking them is a separate, explicit step (the caller decides); no plugin code runs and no
//!   trust is granted.
//!
//! *Unverified:* Herdr's on-disk layout for per-plugin config/state. The planner accepts the
//! layouts listed in [`LAYOUTS`] plus explicit `config_dir`/`state_dir` fields in a registry
//! entry, and reports which one matched.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::registry::{PluginDirs, sha256_hex};

/// Candidate per-plugin layouts under the source root: `(config pattern, state pattern)` with
/// `{id}` replaced by the plugin id.
pub const LAYOUTS: &[(&str, &str)] = &[
    ("plugins/{id}/config", "plugins/{id}/state"),
    ("plugins/config/{id}", "plugins/state/{id}"),
    ("plugin-config/{id}", "plugin-state/{id}"),
];

/// A plugin registration found in the source registry.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SourcePlugin {
    pub id: String,
    /// Plugin root (checkout or linked directory), when the registry records one.
    pub root: Option<PathBuf>,
    pub enabled: Option<bool>,
    /// Whether `root` holds a `herdr-plugin.toml` (so it can be linked/installed).
    pub has_manifest: bool,
}

/// What happens to one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// New file: copied.
    Copy,
    /// Same content already at the destination.
    Same,
    /// Different content at the destination: left alone.
    Conflict,
    /// A symlink or special file in the source: not followed, not copied.
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FileOp {
    pub plugin: String,
    /// `config` or `state`.
    pub area: String,
    pub from: PathBuf,
    pub to: PathBuf,
    pub action: Action,
}

/// The migration plan (also the dry-run report).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Plan {
    pub from: PathBuf,
    pub registry_found: bool,
    pub plugins: Vec<SourcePlugin>,
    /// Which source layout each plugin's config/state came from.
    pub layouts: BTreeMap<String, String>,
    pub files: Vec<FileOp>,
    pub warnings: Vec<String>,
}

impl Plan {
    pub fn count(&self, a: Action) -> usize {
        self.files.iter().filter(|f| f.action == a).count()
    }
}

/// One created path, for rollback.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Created {
    pub path: PathBuf,
    /// `None` for directories.
    pub sha256: Option<String>,
}

/// The record written after applying a plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub from: PathBuf,
    pub at_ms: i64,
    pub created: Vec<Created>,
    pub conflicts: Vec<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum MigrateError {
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("{0}")]
    Invalid(String),
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Plugin ids are used as one path component.
fn safe_id(id: &str) -> bool {
    !id.is_empty() && id != "." && id != ".." && !id.contains(['/', '\\', '\0']) && id.len() <= 200
}

/// Read the source registry tolerantly: `{plugins: {id: entry}}`, `{plugins: [entry]}`,
/// `{id: entry}` or `[entry]`, where an entry may carry `id`, `root`/`path`/`checkout`/
/// `source`, `enabled`, `config_dir`, `state_dir`.
fn read_registry(from: &Path, warnings: &mut Vec<String>) -> Option<Vec<(String, Value)>> {
    let path = from.join("plugins.json");
    let text = std::fs::read_to_string(&path).ok()?;
    let v: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            warnings.push(format!("{}: not valid JSON ({e}); ignored", path.display()));
            return Some(vec![]);
        }
    };
    let body = v.get("plugins").cloned().unwrap_or(v);
    let mut out = Vec::new();
    match body {
        Value::Object(m) => {
            for (k, e) in m {
                let id = e
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or(k);
                out.push((id, e));
            }
        }
        Value::Array(a) => {
            for e in a {
                match e.get("id").and_then(Value::as_str) {
                    Some(id) => out.push((id.to_string(), e.clone())),
                    None => warnings.push("registry entry without an id ignored".into()),
                }
            }
        }
        _ => warnings.push(format!("{}: unexpected shape; ignored", path.display())),
    }
    Some(out)
}

fn entry_path(from: &Path, e: &Value, keys: &[&str]) -> Option<PathBuf> {
    keys.iter()
        .find_map(|k| e.get(*k).and_then(Value::as_str))
        .map(|p| {
            let p = PathBuf::from(p);
            if p.is_absolute() { p } else { from.join(p) }
        })
}

/// Walk `src` (no symlink following) and plan copies into `dst`.
fn plan_tree(
    plugin: &str,
    area: &str,
    src: &Path,
    dst: &Path,
    out: &mut Vec<FileOp>,
) -> io::Result<()> {
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let dir = src.join(&rel);
        let mut entries: Vec<_> = std::fs::read_dir(&dir)?.collect::<Result<_, _>>()?;
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let ft = e.file_type()?;
            let r = rel.join(e.file_name());
            let (from, to) = (src.join(&r), dst.join(&r));
            if ft.is_dir() {
                stack.push(r);
                continue;
            }
            let action = if !ft.is_file() {
                Action::Skipped
            } else {
                match std::fs::symlink_metadata(&to) {
                    Err(_) => Action::Copy,
                    Ok(m) if m.is_file() => {
                        if std::fs::read(&to)? == std::fs::read(&from)? {
                            Action::Same
                        } else {
                            Action::Conflict
                        }
                    }
                    Ok(_) => Action::Conflict,
                }
            };
            out.push(FileOp {
                plugin: plugin.into(),
                area: area.into(),
                from,
                to,
                action,
            });
        }
    }
    Ok(())
}

/// Build the plan for migrating `from` (a Herdr config directory, or a copy of one) into
/// `dirs`. Reads only. `only` restricts it to some plugin ids.
pub fn plan(from: &Path, dirs: &PluginDirs, only: &[String]) -> Result<Plan, MigrateError> {
    let meta = std::fs::metadata(from)
        .map_err(|e| MigrateError::Invalid(format!("{}: {e}", from.display())))?;
    if !meta.is_dir() {
        return Err(MigrateError::Invalid(format!(
            "{} is not a directory",
            from.display()
        )));
    }
    let from = from.canonicalize()?;
    for d in [&dirs.config, &dirs.state] {
        let d = d.canonicalize().unwrap_or_else(|_| d.clone());
        if d.starts_with(&from) || from.starts_with(&d) {
            return Err(MigrateError::Invalid(format!(
                "source {} overlaps Vibeke's plugin directory {}",
                from.display(),
                d.display()
            )));
        }
    }
    let mut warnings = Vec::new();
    let reg = read_registry(&from, &mut warnings);
    let registry_found = reg.is_some();
    let mut entries: BTreeMap<String, Value> = BTreeMap::new();
    for (id, e) in reg.unwrap_or_default() {
        if safe_id(&id) {
            entries.insert(id, e);
        } else {
            warnings.push(format!("unsafe plugin id {id:?} ignored"));
        }
    }
    // Plugins with config/state under a known layout but no registry entry still migrate.
    for (cfg, st) in LAYOUTS {
        for pat in [cfg, st] {
            let (parent, rest) = pat.split_once("{id}").unwrap_or((pat, ""));
            let Ok(rd) = std::fs::read_dir(from.join(parent)) else {
                continue;
            };
            for e in rd.flatten() {
                let id = e.file_name().to_string_lossy().into_owned();
                if safe_id(&id)
                    && from
                        .join(parent)
                        .join(&id)
                        .join(rest.trim_start_matches('/'))
                        .is_dir()
                    && !matches!(id.as_str(), "config" | "state")
                {
                    entries.entry(id).or_insert(Value::Null);
                }
            }
        }
    }
    let mut plugins = Vec::new();
    let mut layouts = BTreeMap::new();
    let mut files = Vec::new();
    for (id, e) in &entries {
        if !only.is_empty() && !only.contains(id) {
            continue;
        }
        let root = entry_path(&from, e, &["root", "path", "checkout", "source"]);
        plugins.push(SourcePlugin {
            id: id.clone(),
            has_manifest: root
                .as_ref()
                .is_some_and(|r| r.join(super::MANIFEST_FILE).is_file()),
            root,
            enabled: e.get("enabled").and_then(Value::as_bool),
        });
        let explicit = (
            entry_path(&from, e, &["config_dir"]),
            entry_path(&from, e, &["state_dir"]),
        );
        let (cfg, st, which) = if explicit.0.is_some() || explicit.1.is_some() {
            (explicit.0, explicit.1, "registry".to_string())
        } else {
            match LAYOUTS.iter().find(|(c, s)| {
                from.join(c.replace("{id}", id)).is_dir()
                    || from.join(s.replace("{id}", id)).is_dir()
            }) {
                Some((c, s)) => (
                    Some(from.join(c.replace("{id}", id))),
                    Some(from.join(s.replace("{id}", id))),
                    format!("{c} + {s}"),
                ),
                None => (None, None, "none".to_string()),
            }
        };
        layouts.insert(id.clone(), which);
        for (area, src, dst) in [
            ("config", cfg, dirs.config_dir(id)),
            ("state", st, dirs.state_dir(id)),
        ] {
            if let Some(src) = src.filter(|s| s.is_dir()) {
                if std::fs::symlink_metadata(&src).is_ok_and(|m| m.file_type().is_symlink()) {
                    warnings.push(format!("{} is a symlink; not followed", src.display()));
                    continue;
                }
                plan_tree(id, area, &src, &dst, &mut files)?;
            }
        }
    }
    Ok(Plan {
        from,
        registry_found,
        plugins,
        layouts,
        files,
        warnings,
    })
}

/// Copy the plan's new files. Never overwrites, never touches the source. Returns the record
/// (also written to `<records>/<id>.json`).
pub fn apply(plan: &Plan, records: &Path) -> Result<Record, MigrateError> {
    let mut created = Vec::new();
    let mut conflicts = Vec::new();
    for f in &plan.files {
        match f.action {
            Action::Conflict => conflicts.push(f.to.clone()),
            Action::Copy => {
                // Re-check at copy time: a file that appeared since planning is a conflict.
                if std::fs::symlink_metadata(&f.to).is_ok() {
                    conflicts.push(f.to.clone());
                    continue;
                }
                let mut missing = Vec::new();
                let mut d = f.to.parent();
                while let Some(p) = d {
                    if p.exists() {
                        break;
                    }
                    missing.push(p.to_path_buf());
                    d = p.parent();
                }
                for p in missing.iter().rev() {
                    std::fs::create_dir(p)?;
                    created.push(Created {
                        path: p.clone(),
                        sha256: None,
                    });
                }
                let bytes = std::fs::read(&f.from)?;
                let mut opts = std::fs::OpenOptions::new();
                opts.write(true).create_new(true);
                {
                    use std::io::Write;
                    let mut out = opts.open(&f.to)?;
                    out.write_all(&bytes)?;
                }
                if let Ok(m) = std::fs::metadata(&f.from) {
                    let _ = std::fs::set_permissions(&f.to, m.permissions());
                }
                created.push(Created {
                    path: f.to.clone(),
                    sha256: Some(sha256_hex(&bytes)),
                });
            }
            Action::Same | Action::Skipped => {}
        }
    }
    let rec = Record {
        id: format!("herdr-{}", now_ms()),
        from: plan.from.clone(),
        at_ms: now_ms(),
        created,
        conflicts,
    };
    std::fs::create_dir_all(records)?;
    let path = records.join(format!("{}.json", rec.id));
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&rec).map_err(|e| MigrateError::Invalid(e.to_string()))?,
    )?;
    Ok(rec)
}

/// The most recent migration record in `records`, if any.
pub fn latest(records: &Path) -> Option<(PathBuf, Record)> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(records)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    v.sort();
    let p = v.pop()?;
    let rec = serde_json::from_slice(&std::fs::read(&p).ok()?).ok()?;
    Some((p, rec))
}

/// Result of a rollback.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Rollback {
    pub removed: Vec<PathBuf>,
    /// Created files changed since the migration: kept.
    pub kept_changed: Vec<PathBuf>,
    /// Created directories that are no longer empty: kept.
    pub kept_dirs: Vec<PathBuf>,
}

/// Undo a migration: remove the files it created that are unchanged, then the directories it
/// created that are empty. The source was never modified, so Herdr's data is as it was.
pub fn rollback(rec: &Record) -> Rollback {
    let mut r = Rollback::default();
    for c in rec.created.iter().filter(|c| c.sha256.is_some()) {
        match std::fs::read(&c.path) {
            Ok(b) if Some(sha256_hex(&b)) == c.sha256 => {
                if std::fs::remove_file(&c.path).is_ok() {
                    r.removed.push(c.path.clone());
                }
            }
            Ok(_) => r.kept_changed.push(c.path.clone()),
            Err(_) => {}
        }
    }
    for c in rec.created.iter().rev().filter(|c| c.sha256.is_none()) {
        if std::fs::remove_dir(&c.path).is_ok() {
            r.removed.push(c.path.clone());
        } else if c.path.exists() {
            r.kept_dirs.push(c.path.clone());
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs(root: &Path) -> PluginDirs {
        PluginDirs {
            registry: root.join("vk/plugins.json"),
            checkouts: root.join("vk/checkouts"),
            config: root.join("vk/plugins"),
            state: root.join("vk/state"),
        }
    }

    fn snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                let p = e.path();
                if e.file_type().unwrap().is_dir() {
                    stack.push(p);
                } else if e.file_type().unwrap().is_file() {
                    out.push((p.clone(), std::fs::read(&p).unwrap()));
                } else {
                    out.push((p.clone(), vec![]));
                }
            }
        }
        out.sort();
        out
    }

    /// A fake Herdr config dir: registry with a linked plugin, config/state in two layouts.
    fn fixture(root: &Path) -> PathBuf {
        let h = root.join("herdr-copy");
        let src = root.join("src/notes");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("herdr-plugin.toml"), "id = \"acme.notes\"\n").unwrap();
        std::fs::create_dir_all(h.join("plugins/acme.notes/config")).unwrap();
        std::fs::create_dir_all(h.join("plugins/acme.notes/state/db")).unwrap();
        std::fs::create_dir_all(h.join("plugins/config/acme.radar")).unwrap();
        std::fs::write(
            h.join("plugins/acme.notes/config/settings.toml"),
            "theme = 1\n",
        )
        .unwrap();
        std::fs::write(h.join("plugins/acme.notes/state/db/notes.json"), "[1,2]").unwrap();
        std::fs::write(h.join("plugins/acme.notes/state/conflict.txt"), "herdr").unwrap();
        std::fs::write(h.join("plugins/config/acme.radar/r.toml"), "x = 2\n").unwrap();
        std::os::unix::fs::symlink("/etc/passwd", h.join("plugins/acme.notes/state/link")).unwrap();
        std::fs::write(
            h.join("plugins.json"),
            serde_json::json!({"plugins": {"acme.notes": {"root": src, "enabled": true}}})
                .to_string(),
        )
        .unwrap();
        h
    }

    #[test]
    fn plan_apply_rollback_never_touch_the_source() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        let h = fixture(&root);
        let d = dirs(&root);
        // A pre-existing Vibeke state file with different content: conflict.
        std::fs::create_dir_all(d.state_dir("acme.notes")).unwrap();
        std::fs::write(d.state_dir("acme.notes").join("conflict.txt"), "vibeke").unwrap();
        let before = snapshot(&h);

        let p = plan(&h, &d, &[]).unwrap();
        assert!(p.registry_found);
        let notes = p.plugins.iter().find(|x| x.id == "acme.notes").unwrap();
        assert!(notes.has_manifest && notes.enabled == Some(true));
        assert!(p.plugins.iter().any(|x| x.id == "acme.radar"), "{p:?}");
        assert_eq!(
            p.layouts["acme.radar"],
            "plugins/config/{id} + plugins/state/{id}"
        );
        assert_eq!(p.count(Action::Copy), 3, "{:#?}", p.files);
        assert_eq!(p.count(Action::Conflict), 1);
        assert_eq!(p.count(Action::Skipped), 1, "symlink not followed");

        let records = root.join("vk/migrations");
        let rec = apply(&p, &records).unwrap();
        assert_eq!(snapshot(&h), before, "source is read-only");
        assert_eq!(
            std::fs::read_to_string(d.state_dir("acme.notes").join("db/notes.json")).unwrap(),
            "[1,2]"
        );
        assert_eq!(
            std::fs::read_to_string(d.state_dir("acme.notes").join("conflict.txt")).unwrap(),
            "vibeke",
            "conflicts are never overwritten"
        );
        assert!(!d.state_dir("acme.notes").join("link").exists());
        assert_eq!(rec.conflicts.len(), 1);
        assert_eq!(latest(&records).unwrap().1, rec);

        // A second plan sees the copies as identical.
        let p2 = plan(&h, &d, &[]).unwrap();
        assert_eq!(p2.count(Action::Copy), 0);
        assert_eq!(p2.count(Action::Same), 3);

        // Rollback removes only unchanged created files and empty created dirs.
        std::fs::write(d.config_dir("acme.radar").join("r.toml"), "edited").unwrap();
        let rb = rollback(&rec);
        assert!(
            rb.kept_changed
                .contains(&d.config_dir("acme.radar").join("r.toml"))
        );
        assert!(!d.config_dir("acme.notes").exists(), "{rb:?}");
        assert!(!d.state_dir("acme.notes").join("db").exists());
        assert!(d.state_dir("acme.notes").join("conflict.txt").exists());
        assert_eq!(snapshot(&h), before);
    }

    #[test]
    fn filters_and_refusals() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        let h = fixture(&root);
        let d = dirs(&root);
        let p = plan(&h, &d, &["acme.radar".to_string()]).unwrap();
        assert_eq!(p.plugins.len(), 1);
        assert!(p.files.iter().all(|f| f.plugin == "acme.radar"));
        assert!(plan(&root.join("nope"), &d, &[]).is_err());
        // The source may not overlap Vibeke's own dirs.
        assert!(plan(&root.join("vk"), &d, &[]).is_err() || !root.join("vk").exists());
        std::fs::create_dir_all(root.join("vk/plugins")).unwrap();
        assert!(plan(&root.join("vk"), &d, &[]).is_err());
        // A corrupt registry is a warning, not an error.
        std::fs::write(h.join("plugins.json"), "{nope").unwrap();
        let p = plan(&h, &d, &[]).unwrap();
        assert!(p.warnings.iter().any(|w| w.contains("not valid JSON")));
    }
}
