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
//! * Paths are canonicalized. Source config/state directories (layout or registry
//!   `config_dir`/`state_dir`) must resolve inside `--from`; others are reported and skipped.
//!   Destination roots are canonicalized at plan time and no component below them may be a
//!   symlink, at plan time, before every directory/file the migration creates (files are opened
//!   `O_EXCL|O_NOFOLLOW`) and again before rollback deletes anything, so a redirected
//!   destination can never write into, or delete from, live Herdr storage.
//! * Every created path is journaled (`<id>.journal`, one JSON line, synced) as soon as it
//!   exists, so a migration that fails half-way can still be rolled back; a retry from the same
//!   source resumes the incomplete record instead of starting a second one.
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
    /// Canonical destination roots (Vibeke's plugin config and state dirs); every planned
    /// destination lies below one of them.
    pub dest_roots: Vec<PathBuf>,
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

fn yes() -> bool {
    true
}

/// The record of one migration: written when it starts, completed when it finishes. Created
/// paths are journaled next to it while it runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub from: PathBuf,
    pub at_ms: i64,
    pub created: Vec<Created>,
    pub conflicts: Vec<PathBuf>,
    /// Destination roots the created paths must stay below (rollback refuses anything else).
    #[serde(default)]
    pub dest_roots: Vec<PathBuf>,
    /// `false` while running, and for a migration that failed part-way.
    #[serde(default = "yes")]
    pub complete: bool,
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
    let mut bases = Vec::new();
    for d in [&dirs.config, &dirs.state] {
        // Vibeke's own plugin dirs are never symlinks; one that is may point anywhere.
        if std::fs::symlink_metadata(d).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(MigrateError::Invalid(format!(
                "{} is a symlink; refusing to migrate into it",
                d.display()
            )));
        }
        let d = canonical_base(d)?;
        if herdr_owned(&d) {
            return Err(MigrateError::Invalid(format!(
                "{} resolves into Herdr's own directory",
                d.display()
            )));
        }
        if d.starts_with(&from) || from.starts_with(&d) {
            return Err(MigrateError::Invalid(format!(
                "source {} overlaps Vibeke's plugin directory {}",
                from.display(),
                d.display()
            )));
        }
        check_below(&d, &d, false).map_err(MigrateError::Invalid)?;
        bases.push(d);
    }
    let (cfg_base, st_base) = (bases[0].clone(), bases[1].clone());
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
        for (area, src, base) in [("config", cfg, &cfg_base), ("state", st, &st_base)] {
            let Some(src) = src.and_then(|s| source_dir(&from, &s, &mut warnings)) else {
                continue;
            };
            let dst = base.join(id);
            plan_tree(id, area, &src, &dst, &mut files)?;
            for f in files
                .iter_mut()
                .filter(|f| f.plugin == *id && f.area == area)
            {
                if f.action != Action::Skipped
                    && let Err(e) = check_below(base, &f.to, true)
                {
                    warnings.push(e);
                    f.action = Action::Conflict;
                }
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
        dest_roots: bases,
    })
}

/// Inside Herdr's own config directory (`~/.config/herdr`, `$XDG_CONFIG_HOME/herdr`), as given
/// or resolved.
fn herdr_owned(p: &Path) -> bool {
    let mut roots = Vec::new();
    if let Some(h) = std::env::var_os("HOME") {
        roots.push(PathBuf::from(h).join(".config/herdr"));
    }
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME") {
        roots.push(PathBuf::from(x).join("herdr"));
    }
    roots
        .iter()
        .any(|r| p.starts_with(r) || r.canonicalize().is_ok_and(|c| p.starts_with(c)))
}

/// `path` with its longest existing ancestor canonicalized and the missing rest appended.
pub(crate) fn canonical_base(path: &Path) -> io::Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut rest = Vec::new();
    let mut cur = path.clone();
    loop {
        match cur.canonicalize() {
            Ok(mut c) => {
                for r in rest.iter().rev() {
                    c.push(r);
                }
                return Ok(c);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let (Some(name), Some(parent)) = (cur.file_name(), cur.parent()) else {
                    return Err(io::Error::other(format!(
                        "{}: invalid path",
                        path.display()
                    )));
                };
                rest.push(name.to_owned());
                cur = parent.to_path_buf();
            }
            Err(e) => return Err(e),
        }
    }
}

/// `path` lies lexically below (or at) `base` with only normal components, and no existing
/// component from `base` down to `path` is a symlink: each is a real directory, except that the
/// leaf may be a regular file when `leaf_file_ok`.
fn check_below(base: &Path, path: &Path, leaf_file_ok: bool) -> Result<(), String> {
    use std::path::Component;
    let rel = path
        .strip_prefix(base)
        .map_err(|_| format!("{} is outside {}", path.display(), base.display()))?;
    if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(format!("{}: unexpected path component", path.display()));
    }
    let n = rel.components().count();
    let mut cur = base.to_path_buf();
    for i in 0..=n {
        if i > 0 {
            cur.push(rel.components().nth(i - 1).unwrap());
        }
        match std::fs::symlink_metadata(&cur) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(format!("{}: {e}", cur.display())),
            Ok(m) if m.file_type().is_symlink() => {
                return Err(format!(
                    "{} is a symlink; refusing to write through it",
                    cur.display()
                ));
            }
            Ok(m) if m.is_dir() => {}
            Ok(m) if m.is_file() && i == n && leaf_file_ok => {}
            Ok(_) => return Err(format!("{} is not a directory", cur.display())),
        }
    }
    Ok(())
}

/// A source config/state directory: not a symlink, and resolving inside `from`.
fn source_dir(from: &Path, p: &Path, warnings: &mut Vec<String>) -> Option<PathBuf> {
    let m = std::fs::symlink_metadata(p).ok()?;
    if m.file_type().is_symlink() {
        warnings.push(format!("{} is a symlink; not followed", p.display()));
        return None;
    }
    if !m.is_dir() {
        return None;
    }
    let c = p.canonicalize().ok()?;
    if !c.starts_with(from) {
        warnings.push(format!(
            "{} is outside the source directory {}; skipped",
            p.display(),
            from.display()
        ));
        return None;
    }
    Some(c)
}

/// The journal of created paths next to a record (`<id>.journal`).
fn journal_path(record: &Path) -> PathBuf {
    record.with_extension("journal")
}

fn write_record(path: &Path, rec: &Record) -> Result<(), MigrateError> {
    let bytes = serde_json::to_vec_pretty(rec).map_err(|e| MigrateError::Invalid(e.to_string()))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Appends each created path to the journal (and syncs) before the next step runs.
struct Journal {
    file: std::fs::File,
    created: Vec<Created>,
}

impl Journal {
    fn add(&mut self, c: Created) -> Result<(), MigrateError> {
        use std::io::Write;
        let mut line = serde_json::to_vec(&c).map_err(|e| MigrateError::Invalid(e.to_string()))?;
        line.push(b'\n');
        self.file.write_all(&line)?;
        self.file.sync_data()?;
        self.created.push(c);
        Ok(())
    }
}

/// Open a file without following a symlink at its final component.
fn open_nofollow(opts: &mut std::fs::OpenOptions, path: &Path) -> io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    opts.open(path)
}

/// Copy the plan's new files. Never overwrites, never touches the source, never writes through
/// a symlink below a destination root. The record is written when the migration starts and
/// every created path is journaled immediately, so a migration that fails part-way can be rolled
/// back; a retry from the same source continues that incomplete record. Returns the completed
/// record (`<records>/<id>.json`).
pub fn apply(plan: &Plan, records: &Path) -> Result<Record, MigrateError> {
    std::fs::create_dir_all(records)?;
    let (path, mut rec) = match latest(records) {
        Some((p, r)) if !r.complete && r.from == plan.from => (p, r),
        _ => {
            let id = format!("herdr-{}", now_ms());
            let rec = Record {
                id: id.clone(),
                from: plan.from.clone(),
                at_ms: now_ms(),
                created: vec![],
                conflicts: vec![],
                dest_roots: plan.dest_roots.clone(),
                complete: false,
            };
            (records.join(format!("{id}.json")), rec)
        }
    };
    for r in &plan.dest_roots {
        if !rec.dest_roots.contains(r) {
            rec.dest_roots.push(r.clone());
        }
    }
    write_record(
        &path,
        &Record {
            created: vec![],
            ..rec.clone()
        },
    )?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(journal_path(&path))?;
    let mut j = Journal {
        file,
        created: rec.created.clone(),
    };
    let mut conflicts = Vec::new();
    for f in &plan.files {
        match f.action {
            Action::Conflict => conflicts.push(f.to.clone()),
            Action::Copy => {
                let base = plan
                    .dest_roots
                    .iter()
                    .find(|b| f.to.starts_with(b))
                    .ok_or_else(|| {
                        MigrateError::Invalid(format!(
                            "{} is outside Vibeke's plugin directories",
                            f.to.display()
                        ))
                    })?;
                // Re-check at copy time: a redirected ancestor is refused, a file that appeared
                // since planning is a conflict.
                check_below(base, &f.to, true).map_err(MigrateError::Invalid)?;
                if std::fs::symlink_metadata(&f.to).is_ok() {
                    conflicts.push(f.to.clone());
                    continue;
                }
                // Vibeke's own plugin dir (kept, empty, by rollback).
                std::fs::create_dir_all(base)?;
                check_below(base, base, false).map_err(MigrateError::Invalid)?;
                let rel = f.to.strip_prefix(base).unwrap_or(Path::new(""));
                let mut cur = base.clone();
                let parts: Vec<_> = rel.components().collect();
                for c in &parts[..parts.len().saturating_sub(1)] {
                    cur.push(c);
                    check_below(base, &cur, false).map_err(MigrateError::Invalid)?;
                    match std::fs::create_dir(&cur) {
                        Ok(()) => j.add(Created {
                            path: cur.clone(),
                            sha256: None,
                        })?,
                        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                        Err(e) => return Err(e.into()),
                    }
                }
                check_below(base, &f.to, true).map_err(MigrateError::Invalid)?;
                let bytes = {
                    use std::io::Read;
                    let mut src = open_nofollow(std::fs::OpenOptions::new().read(true), &f.from)?;
                    let mut b = Vec::new();
                    src.read_to_end(&mut b)?;
                    b
                };
                let mut out = open_nofollow(
                    std::fs::OpenOptions::new().write(true).create_new(true),
                    &f.to,
                )?;
                // Journaled as soon as it exists (empty or not), so rollback can find it.
                let sha = sha256_hex(&bytes);
                j.add(Created {
                    path: f.to.clone(),
                    sha256: Some(sha),
                })?;
                if let Err(e) = std::io::Write::write_all(&mut out, &bytes) {
                    // Ours (created exclusively above): never leave a partial copy behind.
                    let _ = std::fs::remove_file(&f.to);
                    return Err(e.into());
                }
                if let Ok(m) = std::fs::metadata(&f.from) {
                    let _ = out.set_permissions(m.permissions());
                }
            }
            Action::Same | Action::Skipped => {}
        }
    }
    rec.created = j.created;
    rec.conflicts = conflicts;
    rec.complete = true;
    write_record(&path, &rec)?;
    let _ = std::fs::remove_file(journal_path(&path));
    Ok(rec)
}

/// The most recent migration record in `records`, if any, including the paths journaled by a
/// migration that did not finish.
pub fn latest(records: &Path) -> Option<(PathBuf, Record)> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(records)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    v.sort();
    let p = v.pop()?;
    let mut rec: Record = serde_json::from_slice(&std::fs::read(&p).ok()?).ok()?;
    if let Ok(text) = std::fs::read_to_string(journal_path(&p)) {
        for c in text
            .lines()
            .filter_map(|l| serde_json::from_str::<Created>(l).ok())
        {
            if !rec.created.contains(&c) {
                rec.created.push(c);
            }
        }
    }
    Some((p, rec))
}

/// Mark a rolled-back record (and its journal) as done so it is not rolled back again.
pub fn retire(record: &Path) {
    let _ = std::fs::rename(record, record.with_extension("rolled-back"));
    let j = journal_path(record);
    if j.exists() {
        let _ = std::fs::rename(&j, record.with_extension("journal.rolled-back"));
    }
}

/// Result of a rollback.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Rollback {
    pub removed: Vec<PathBuf>,
    /// Created files changed since the migration: kept.
    pub kept_changed: Vec<PathBuf>,
    /// Created directories that are no longer empty: kept.
    pub kept_dirs: Vec<PathBuf>,
    /// Paths that are no longer safe to touch (outside the destination roots, inside the
    /// source, behind a symlink or replaced by one): never deleted.
    pub refused: Vec<PathBuf>,
}

/// Whether rollback may touch `path`: below one of the record's destination roots with no
/// symlink on the way (or, for records without roots, a path that resolves to itself), and not
/// inside the migration's source.
fn rollback_safe(rec: &Record, path: &Path) -> bool {
    if path.starts_with(&rec.from) {
        return false;
    }
    if rec.dest_roots.is_empty() {
        return path
            .parent()
            .is_some_and(|p| p.canonicalize().ok().as_deref() == Some(p));
    }
    rec.dest_roots
        .iter()
        .any(|b| path != b.as_path() && check_below(b, path, true).is_ok())
}

/// Undo a migration: remove the files it created that are unchanged, then the directories it
/// created that are empty. Every path is validated first ([`rollback_safe`]); a symlink is never
/// followed or removed. The source was never modified, so Herdr's data is as it was.
pub fn rollback(rec: &Record) -> Rollback {
    let mut r = Rollback::default();
    for c in rec.created.iter().filter(|c| c.sha256.is_some()) {
        let Ok(m) = std::fs::symlink_metadata(&c.path) else {
            continue;
        };
        if !m.is_file() || !rollback_safe(rec, &c.path) {
            r.refused.push(c.path.clone());
            continue;
        }
        let bytes =
            open_nofollow(std::fs::OpenOptions::new().read(true), &c.path).and_then(|mut f| {
                use std::io::Read;
                let mut b = Vec::new();
                f.read_to_end(&mut b).map(|_| b)
            });
        match bytes {
            Ok(b) if Some(sha256_hex(&b)) == c.sha256 => {
                if std::fs::remove_file(&c.path).is_ok() {
                    r.removed.push(c.path.clone());
                }
            }
            Ok(_) => r.kept_changed.push(c.path.clone()),
            Err(_) => r.refused.push(c.path.clone()),
        }
    }
    for c in rec.created.iter().rev().filter(|c| c.sha256.is_none()) {
        let Ok(m) = std::fs::symlink_metadata(&c.path) else {
            continue;
        };
        if !m.is_dir() || !rollback_safe(rec, &c.path) {
            r.refused.push(c.path.clone());
            continue;
        }
        if std::fs::remove_dir(&c.path).is_ok() {
            r.removed.push(c.path.clone());
        } else {
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

    /// Live Herdr storage stand-in, outside the migration source.
    fn live(root: &Path) -> PathBuf {
        let l = root.join("live-herdr/plugins/acme.notes/state");
        std::fs::create_dir_all(l.join("db")).unwrap();
        std::fs::write(l.join("db/notes.json"), "[1,2]").unwrap();
        l
    }

    #[test]
    fn destination_symlinks_never_redirect_writes() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        let h = fixture(&root);
        let d = dirs(&root);
        let l = live(&root);
        let before = snapshot(&root.join("live-herdr"));
        // A per-plugin destination dir linked into live storage.
        std::fs::create_dir_all(&d.state).unwrap();
        std::os::unix::fs::symlink(&l, d.state_dir("acme.notes")).unwrap();
        let p = plan(&h, &d, &[]).unwrap();
        assert!(
            p.files
                .iter()
                .filter(|f| f.plugin == "acme.notes"
                    && f.area == "state"
                    && f.action != Action::Skipped)
                .all(|f| f.action == Action::Conflict),
            "{:#?}",
            p.files
        );
        assert!(
            p.warnings.iter().any(|w| w.contains("symlink")),
            "{:?}",
            p.warnings
        );
        let rec = apply(&p, &root.join("vk/migrations")).unwrap();
        assert!(rec.created.iter().all(|c| !c.path.starts_with(&l)));
        assert_eq!(
            snapshot(&root.join("live-herdr")),
            before,
            "live storage untouched"
        );
        // The symlink appearing after planning is refused at copy time.
        std::fs::remove_file(d.state_dir("acme.notes")).unwrap();
        let _ = std::fs::remove_dir_all(d.config_dir("acme.notes"));
        let _ = std::fs::remove_dir_all(d.config_dir("acme.radar"));
        let p = plan(&h, &d, &[]).unwrap();
        assert!(
            p.files
                .iter()
                .any(|f| f.action == Action::Copy && f.to.starts_with(d.state_dir("acme.notes")))
        );
        std::os::unix::fs::symlink(&l, d.state_dir("acme.notes")).unwrap();
        assert!(apply(&p, &root.join("vk/migrations2")).is_err());
        assert_eq!(snapshot(&root.join("live-herdr")), before);
        // Vibeke's plugin dir itself being a symlink is refused outright.
        let t2 = tempfile::tempdir().unwrap();
        let r2 = t2.path().canonicalize().unwrap();
        let h2 = fixture(&r2);
        let d2 = dirs(&r2);
        std::fs::create_dir_all(r2.join("vk")).unwrap();
        std::os::unix::fs::symlink(&l, &d2.state).unwrap();
        assert!(plan(&h2, &d2, &[]).is_err());
    }

    #[test]
    fn rollback_refuses_redirected_ancestors() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        let h = fixture(&root);
        let d = dirs(&root);
        let l = live(&root);
        let before = snapshot(&root.join("live-herdr"));
        let rec = apply(&plan(&h, &d, &[]).unwrap(), &root.join("vk/migrations")).unwrap();
        // Replace a created directory with a symlink to live storage holding an identical file.
        let db = d.state_dir("acme.notes").join("db");
        std::fs::remove_dir_all(&db).unwrap();
        std::os::unix::fs::symlink(l.join("db"), &db).unwrap();
        let rb = rollback(&rec);
        assert!(rb.refused.contains(&db.join("notes.json")), "{rb:?}");
        assert!(rb.refused.contains(&db), "{rb:?}");
        assert!(l.join("db/notes.json").exists(), "live file not deleted");
        assert_eq!(snapshot(&root.join("live-herdr")), before);
        assert!(
            db.symlink_metadata().unwrap().file_type().is_symlink(),
            "link left alone"
        );
        // A record pointing outside its destination roots or into the source is refused.
        let forged = Record {
            created: vec![
                Created {
                    path: l.join("db/notes.json"),
                    sha256: Some(sha256_hex(b"[1,2]")),
                },
                Created {
                    path: h.join("plugins/acme.notes/state/db/notes.json"),
                    sha256: Some(sha256_hex(b"[1,2]")),
                },
            ],
            ..rec.clone()
        };
        let rb = rollback(&forged);
        assert_eq!(rb.refused.len(), 2, "{rb:?}");
        assert!(rb.removed.is_empty());
        assert_eq!(snapshot(&root.join("live-herdr")), before);
    }

    #[test]
    fn source_registry_paths_must_stay_inside_from() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        let h = fixture(&root);
        let d = dirs(&root);
        let l = live(&root);
        std::fs::create_dir_all(h.join("inside")).unwrap();
        std::fs::write(h.join("inside/ok.toml"), "ok").unwrap();
        std::os::unix::fs::symlink(&l, h.join("escape")).unwrap();
        std::fs::write(
            h.join("plugins.json"),
            serde_json::json!({"plugins": {
                "acme.out": {"config_dir": l.to_string_lossy(), "state_dir": "../live-herdr"},
                "acme.via": {"state_dir": "escape"},
                "acme.in": {"config_dir": "inside"},
            }})
            .to_string(),
        )
        .unwrap();
        let p = plan(&h, &d, &[]).unwrap();
        assert!(
            p.files
                .iter()
                .all(|f| f.plugin != "acme.out" && f.plugin != "acme.via"),
            "{:#?}",
            p.files
        );
        assert!(p.files.iter().any(|f| f.plugin == "acme.in"));
        assert!(
            p.warnings.iter().any(|w| w.contains("outside the source")),
            "{:?}",
            p.warnings
        );
        assert!(
            p.warnings.iter().any(|w| w.contains("symlink")),
            "{:?}",
            p.warnings
        );
    }

    #[test]
    fn a_failed_migration_is_journaled_and_fully_rolled_back_after_a_retry() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        let h = fixture(&root);
        let d = dirs(&root);
        let records = root.join("vk/migrations");
        let bad = h.join("plugins/config/acme.radar/r.toml");
        std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&bad).is_ok() {
            return; // running as root: permissions do not stop reads
        }
        let p = plan(&h, &d, &[]).unwrap();
        assert!(apply(&p, &records).is_err(), "fails part-way");
        let (_, partial) = latest(&records).unwrap();
        assert!(!partial.complete);
        assert!(
            partial
                .created
                .iter()
                .any(|c| c.path == d.config_dir("acme.notes").join("settings.toml")),
            "copies before the failure are journaled: {partial:?}"
        );
        // Retry: resumes the same record.
        std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o644)).unwrap();
        let p = plan(&h, &d, &[]).unwrap();
        let rec = apply(&p, &records).unwrap();
        assert_eq!(rec.id, partial.id, "the incomplete record is continued");
        assert!(rec.complete);
        let (path, last) = latest(&records).unwrap();
        assert_eq!(last, rec);
        let rb = rollback(&last);
        assert!(
            rb.refused.is_empty() && rb.kept_changed.is_empty(),
            "{rb:?}"
        );
        for x in ["acme.notes", "acme.radar"] {
            assert!(!d.config_dir(x).exists(), "{x} config removed: {rb:?}");
        }
        assert!(!d.state_dir("acme.notes").join("db").exists());
        retire(&path);
        assert!(latest(&records).is_none());
    }
}
