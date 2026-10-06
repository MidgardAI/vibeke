//! The per-user plugin registry with explicit Herdr legacy trust (07 §7.7, 09 §6).
//!
//! * Registrations are per user and shared by every session on the machine, stored atomically
//!   in `plugins.json` next to Vibeke's `config.toml` (default `~/.config/vibeke/plugins.json`).
//!   Installs and links work while no server is running; servers re-read the file.
//! * `install` copies a local plugin directory (or a fetched repository) into a fresh, immutable
//!   managed checkout `<checkouts>/<id>/<commit|local>-<nonce>/`; `link` registers a directory
//!   in place. Neither runs any plugin code. The new checkout is staged and verified first and
//!   only then published by saving the registry (new root, commit and the kept or revoked grant
//!   in one atomic write); until that save the registry still names the old checkout, which is
//!   never modified, so a crash or a failed save leaves the old reviewed content in use.
//!   Replaced checkouts are garbage-collected by a later install ([`CHECKOUT_GRACE`]).
//! * Symlinks: an install refuses any symlink that resolves outside the checkout; an entrypoint
//!   that resolves outside the plugin root is refused at trust and makes the status `broken`;
//!   an internal symlink's resolved target contents are pinned by the entry digest.
//! * Launches re-verify the whole-tree digest of a managed checkout against the grant
//!   ([`verify_launch`]; cached by a stat fingerprint of the tree), so a dependency changed in
//!   place after review never runs under the old grant.
//! * A Herdr plugin is **inactive until trusted**: `trust` records a `herdr_legacy` grant bound to
//!   the manifest's SHA-256, the root, the source path, a content digest of the whole reviewed
//!   tree and a digest of every file the manifest's commands reference. Any manifest change, a
//!   changed referenced file or a different source makes the grant stale (re-review required).
//!   A reinstall keeps the grant only when the source and the whole tree are unchanged and the
//!   plugin needs no build; otherwise it is inactive until reviewed and rebuilt. Each grant has
//!   a unique `grant_id`, so a revoke followed by a new grant never revives old invocations.
//!   Unlink/uninstall drop the registration and its grant.
//! * Every change is a read-modify-write under an exclusive lock ([`Registry::update`]), so
//!   concurrent CLI processes and servers never lose each other's decisions (a revocation made
//!   while another plugin builds stays revoked).
//! * Herdr's own registry (`~/.config/herdr/plugins.json`) is never read or written here.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::manifest::{Manifest, ManifestError};
use super::{BASELINE_VERSION, MANIFEST_FILE};

/// Where registrations, managed checkouts and plugin-owned config/state live.
#[derive(Debug, Clone)]
pub struct PluginDirs {
    /// `plugins.json`.
    pub registry: PathBuf,
    /// Managed checkouts: `<checkouts>/<id>/`.
    pub checkouts: PathBuf,
    /// Plugin config dirs: `<config>/<id>/` (`HERDR_PLUGIN_CONFIG_DIR`).
    pub config: PathBuf,
    /// Plugin state dirs: `<state>/<id>/` (`HERDR_PLUGIN_STATE_DIR`).
    pub state: PathBuf,
}

impl PluginDirs {
    pub fn config_dir(&self, id: &str) -> PathBuf {
        self.config.join(id)
    }
    pub fn state_dir(&self, id: &str) -> PathBuf {
        self.state.join(id)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error("plugin not found: {0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error("plugins.json is corrupt: {0}")]
    Corrupt(String),
}

/// The explicit broad grant a Herdr plugin needs before any of its code runs (09 §6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Grant {
    /// Always `herdr_legacy` in this slice.
    pub mode: String,
    pub manifest_sha256: String,
    pub root: PathBuf,
    pub granted_at_ms: i64,
    /// Entrypoints shown at grant time.
    pub entrypoints: Vec<String>,
    /// Herdr baseline the grant was reviewed against.
    pub baseline: String,
    /// Unique per grant: a revoke + new grant never matches an invocation of the old one.
    #[serde(default)]
    pub grant_id: String,
    /// The registration's source (`origin.path`) at review time.
    #[serde(default)]
    pub source: PathBuf,
    /// Content digest of the whole reviewed tree ([`tree_digest`]), before any build.
    #[serde(default)]
    pub tree_sha256: String,
    /// Digest of the files the manifest's commands reference ([`entry_digest`]); re-recorded
    /// after a successful build (which may produce them). Checked on every status read.
    #[serde(default)]
    pub entry_sha256: String,
    /// The commit the reviewed checkout was resolved to (`owner/repo` sources). A different
    /// commit makes the grant stale: an update is reviewed again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Whole-tree digest a launch must find: the reviewed tree, re-recorded after a successful
    /// build (which adds its outputs). Checked before every launch of a managed checkout.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub run_tree_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Origin {
    /// `local` (copied from a directory), `git` (`owner/repo`) or `link`.
    pub kind: String,
    /// The directory (local/link) or the repository URL plus subdirectory (git).
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_ref: Option<String>,
    /// `owner/repo[/subdir]` of a repository source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// The commit the checkout was resolved to (repository sources).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    /// `herdr` for `herdr-plugin.toml` plugins.
    pub kind: String,
    pub root: PathBuf,
    /// The checkout is Vibeke-owned (install) rather than the user's directory (link).
    pub managed: bool,
    pub origin: Origin,
    /// The user's enabled intent; effective only with a valid grant.
    pub enabled: bool,
    /// `[[build]]` ran successfully for this checkout.
    #[serde(default)]
    pub built: bool,
    pub installed_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust: Option<Grant>,
}

/// Effective state of a registration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Enabled and trusted: actions, hooks and startup run.
    Active,
    /// Trusted but disabled by the user.
    Disabled,
    /// No legacy grant yet: nothing runs.
    Untrusted,
    /// The manifest changed after the grant: nothing runs until re-reviewed.
    StaleTrust,
    /// The manifest file is missing or no longer parses, or an entrypoint resolves outside the
    /// plugin root.
    Broken,
}

impl Status {
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Active => "active",
            Status::Disabled => "disabled",
            Status::Untrusted => "untrusted",
            Status::StaleTrust => "stale_trust",
            Status::Broken => "broken",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Registry {
    pub version: u32,
    pub plugins: BTreeMap<String, Entry>,
    /// Native `vibeke-plugin.toml` plugins (07 §7.1–7.6, [`crate::native`]): kept in the same
    /// file, lock and generation, but never visible to the Herdr code paths above.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub native: BTreeMap<String, crate::native::registry::NativeEntry>,
    /// Bumped by every committed change ([`Registry::update`]); running sessions reconcile
    /// committed generations (02 §3 "Plugin state ownership").
    #[serde(default)]
    pub generation: u64,
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let d = Sha256::digest(bytes);
    d.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn manifest_file(root: &Path) -> PathBuf {
    root.join(MANIFEST_FILE)
}

/// The manifest and its digest for a registration root.
pub fn read_manifest(root: &Path) -> Result<(Manifest, String), RegistryError> {
    let text = std::fs::read_to_string(manifest_file(root)).map_err(|e| {
        RegistryError::Io(io::Error::new(
            e.kind(),
            format!("{}: {e}", manifest_file(root).display()),
        ))
    })?;
    let m = Manifest::parse(&text)?;
    Ok((m, sha256_hex(text.as_bytes())))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A unique grant id (time, pid and a process-local counter).
pub(crate) fn new_grant_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{nanos:x}-{:x}-{:x}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

#[cfg(unix)]
fn exec_bits(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111
}

#[cfg(not(unix))]
fn exec_bits(_: &std::fs::Metadata) -> u32 {
    0
}

/// Content digest of a plugin tree: every entry below `root` (`.git` excluded) in sorted order,
/// with its relative path, kind, executable bits and content (files) or target (symlinks).
pub fn tree_digest(root: &Path) -> io::Result<String> {
    fn walk(root: &Path, rel: &Path, h: &mut Sha256) -> io::Result<()> {
        let mut entries: Vec<_> = std::fs::read_dir(root.join(rel))?.collect::<Result<_, _>>()?;
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name();
            if rel.as_os_str().is_empty() && name == ".git" {
                continue;
            }
            let r = rel.join(&name);
            let meta = std::fs::symlink_metadata(root.join(&r))?;
            let ft = meta.file_type();
            h.update(r.to_string_lossy().as_bytes());
            h.update([0]);
            if ft.is_symlink() {
                h.update(b"l");
                h.update(
                    std::fs::read_link(root.join(&r))?
                        .to_string_lossy()
                        .as_bytes(),
                );
            } else if ft.is_dir() {
                h.update(b"d");
                walk(root, &r, h)?;
            } else if ft.is_file() {
                h.update(format!("f{:o}", exec_bits(&meta)).as_bytes());
                h.update(Sha256::digest(std::fs::read(root.join(&r))?));
            } else {
                h.update(b"s");
            }
            h.update([0]);
        }
        Ok(())
    }
    let mut h = Sha256::new();
    walk(root, Path::new(""), &mut h)?;
    Ok(hex(&h.finalize()))
}

/// A cheap fingerprint of a tree (no file reads): every entry's relative path, kind, mode,
/// size, inode and modification/change times. Any write, rename, chmod or touch changes it
/// (`ctime` cannot be set back by the writer).
fn tree_fingerprint(root: &Path) -> io::Result<String> {
    fn walk(root: &Path, rel: &Path, h: &mut Sha256) -> io::Result<()> {
        let mut entries: Vec<_> = std::fs::read_dir(root.join(rel))?.collect::<Result<_, _>>()?;
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name();
            if rel.as_os_str().is_empty() && name == ".git" {
                continue;
            }
            let r = rel.join(&name);
            let meta = std::fs::symlink_metadata(root.join(&r))?;
            h.update(r.to_string_lossy().as_bytes());
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                h.update(
                    format!(
                        "\0{}:{}:{}:{}.{}:{}.{}",
                        meta.mode(),
                        meta.len(),
                        meta.ino(),
                        meta.mtime(),
                        meta.mtime_nsec(),
                        meta.ctime(),
                        meta.ctime_nsec()
                    )
                    .as_bytes(),
                );
            }
            #[cfg(not(unix))]
            h.update(format!("\0{}:{:?}", meta.len(), meta.modified().ok()).as_bytes());
            h.update([0]);
            if meta.file_type().is_dir() {
                walk(root, &r, h)?;
            }
        }
        Ok(())
    }
    let mut h = Sha256::new();
    walk(root, Path::new(""), &mut h)?;
    Ok(hex(&h.finalize()))
}

/// [`tree_digest`], reused while the tree's stat fingerprint is unchanged (launches of event
/// hooks are frequent; reading every file each time is not needed to notice a change).
pub fn tree_digest_cached(root: &Path) -> io::Result<String> {
    type Cache = HashMap<PathBuf, (String, String)>;
    static CACHE: std::sync::LazyLock<std::sync::Mutex<Cache>> =
        std::sync::LazyLock::new(Default::default);
    let before = tree_fingerprint(root)?;
    if let Some((fp, d)) = CACHE.lock().unwrap().get(root)
        && *fp == before
    {
        return Ok(d.clone());
    }
    let digest = tree_digest(root)?;
    // Only cache a digest whose tree did not change while it was read.
    if tree_fingerprint(root)? == before {
        CACHE
            .lock()
            .unwrap()
            .insert(root.to_path_buf(), (before, digest.clone()));
    }
    Ok(digest)
}

/// Normalize `rel` (relative to the plugin root) without touching the filesystem; `None` when
/// it climbs above the root.
fn lexical(rel: &Path) -> Option<PathBuf> {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in rel.components() {
        match c {
            Component::Normal(n) => out.push(n),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(out)
}

/// The first symlink below `root` (`.git` excluded) that points outside it: an absolute target,
/// a relative target climbing above the root, or a chain that resolves outside it.
pub fn escaping_symlink(root: &Path) -> io::Result<Option<String>> {
    fn walk(root: &Path, canon: &Path, rel: &Path) -> io::Result<Option<String>> {
        for e in std::fs::read_dir(root.join(rel))? {
            let e = e?;
            let name = e.file_name();
            if rel.as_os_str().is_empty() && name == ".git" {
                continue;
            }
            let r = rel.join(&name);
            let full = root.join(&r);
            let ft = std::fs::symlink_metadata(&full)?.file_type();
            if ft.is_symlink() {
                let target = std::fs::read_link(&full)?;
                let parent = r.parent().unwrap_or(Path::new(""));
                let inside = !target.is_absolute()
                    && lexical(&parent.join(&target)).is_some()
                    && std::fs::canonicalize(&full).map_or(true, |c| c.starts_with(canon));
                if !inside {
                    return Ok(Some(format!("{} -> {}", r.display(), target.display())));
                }
            } else if ft.is_dir()
                && let Some(x) = walk(root, canon, &r)?
            {
                return Ok(Some(x));
            }
        }
        Ok(None)
    }
    let canon = std::fs::canonicalize(root)?;
    walk(root, &canon, Path::new(""))
}

/// Every command line the manifest declares, on any platform.
fn all_commands(m: &Manifest) -> Vec<&[String]> {
    let mut v: Vec<&[String]> = Vec::new();
    v.extend(m.build.iter().map(|s| s.command.as_slice()));
    v.extend(m.startup.iter().map(|s| s.command.as_slice()));
    v.extend(m.actions.iter().map(|a| a.command.as_slice()));
    v.extend(m.events.iter().map(|e| e.command.as_slice()));
    v.extend(m.panes.iter().map(|p| p.command.as_slice()));
    v
}

/// Plugin-relative paths the manifest's commands reference (an argument that is a plain
/// relative path, such as `bin/hook.sh` or `dist/index.js`; whether it exists is checked by the
/// caller).
fn referenced(m: &Manifest) -> std::collections::BTreeSet<PathBuf> {
    use std::path::Component;
    let mut files = std::collections::BTreeSet::new();
    for cmd in all_commands(m) {
        for arg in cmd {
            let p = Path::new(arg.as_str());
            if arg.is_empty() || arg.starts_with('-') || p.is_absolute() {
                continue;
            }
            if p.components()
                .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
            {
                continue;
            }
            files.insert(p.to_path_buf());
        }
    }
    files
}

/// A referenced path that exists but resolves (through a symlink anywhere along it) outside
/// the plugin root: such an entrypoint would run unreviewed, mutable content.
pub fn escaping_entrypoint(root: &Path, m: &Manifest) -> Option<String> {
    let canon = std::fs::canonicalize(root).ok()?;
    referenced(m).into_iter().find_map(|f| {
        let full = root.join(&f);
        std::fs::symlink_metadata(&full).ok()?;
        match std::fs::canonicalize(&full) {
            Ok(c) if c.starts_with(&canon) => None,
            Ok(c) => Some(format!("{} resolves to {}", f.display(), c.display())),
            // A dangling link: its target may appear later, anywhere.
            Err(_) => std::fs::read_link(&full)
                .ok()
                .map(|t| format!("{} -> {} (dangling)", f.display(), t.display())),
        }
    })
}

/// Digest of the files below `root` that the manifest's commands reference, with their
/// executable bits and content. A symlink (the path itself or a directory along it) is hashed
/// with its link text **and** the contents of the file it resolves to, so changing an internal
/// link's target invalidates the grant; a path resolving outside the root is hashed as such (and
/// refused by [`entry_status`]). Cheap enough to recompute on every status read.
pub fn entry_digest(root: &Path, m: &Manifest) -> String {
    let canon = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut h = Sha256::new();
    for f in referenced(m) {
        let full = root.join(&f);
        let Ok(lmeta) = std::fs::symlink_metadata(&full) else {
            continue;
        };
        let resolved = std::fs::canonicalize(&full).ok();
        let via_link = lmeta.file_type().is_symlink()
            || resolved
                .as_ref()
                .is_some_and(|c| c.strip_prefix(&canon).ok() != Some(f.as_path()));
        if let Some(c) = &resolved
            && !c.starts_with(&canon)
        {
            h.update(f.to_string_lossy().as_bytes());
            h.update(b"\0escape\0");
            h.update(c.to_string_lossy().as_bytes());
            h.update([0]);
            continue;
        }
        let Ok(meta) = std::fs::metadata(&full) else {
            // Dangling link: pinned by its text.
            if let Ok(t) = std::fs::read_link(&full) {
                h.update(f.to_string_lossy().as_bytes());
                h.update(b"\0l");
                h.update(t.to_string_lossy().as_bytes());
                h.update([0]);
            }
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        h.update(f.to_string_lossy().as_bytes());
        if via_link {
            h.update(b"\0l");
            if let Ok(t) = std::fs::read_link(&full) {
                h.update(t.to_string_lossy().as_bytes());
            }
            h.update(b"\0");
            if let Some(c) = &resolved
                && let Ok(rel) = c.strip_prefix(&canon)
            {
                h.update(rel.to_string_lossy().as_bytes());
            }
        }
        h.update(format!("\0f{:o}", exec_bits(&meta)).as_bytes());
        match std::fs::read(&full) {
            Ok(b) => h.update(Sha256::digest(b)),
            Err(_) => h.update(b"unreadable"),
        }
        h.update([0]);
    }
    hex(&h.finalize())
}

/// An advisory lock on `plugins.json.lock` (exclusive for changes, shared for launches),
/// held while the guard lives.
pub(crate) struct RegistryLock {
    _file: std::fs::File,
}

impl RegistryLock {
    pub(crate) fn acquire(dirs: &PluginDirs) -> io::Result<Self> {
        Self::acquire_mode(dirs, true)
    }

    pub(crate) fn acquire_mode(dirs: &PluginDirs, exclusive: bool) -> io::Result<Self> {
        if let Some(d) = dirs.registry.parent() {
            std::fs::create_dir_all(d)?;
        }
        let path = dirs.registry.with_extension("json.lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            loop {
                // SAFETY: flock on a descriptor we own; blocks until the lock is available.
                let op = if exclusive {
                    libc::LOCK_EX
                } else {
                    libc::LOCK_SH
                };
                if unsafe { libc::flock(file.as_raw_fd(), op) } == 0 {
                    break;
                }
                let e = io::Error::last_os_error();
                if e.kind() != io::ErrorKind::Interrupted {
                    return Err(e);
                }
            }
        }
        Ok(RegistryLock { _file: file })
    }
}

/// A plugin directory from a path that is either the directory or its manifest.
fn plugin_root(src: &Path) -> Result<PathBuf, RegistryError> {
    let p = if src.is_file() {
        src.parent().unwrap_or(Path::new(".")).to_path_buf()
    } else {
        src.to_path_buf()
    };
    let p = std::fs::canonicalize(&p).map_err(|e| {
        RegistryError::Io(io::Error::new(e.kind(), format!("{}: {e}", p.display())))
    })?;
    if !manifest_file(&p).is_file() {
        return Err(RegistryError::Conflict(format!(
            "{} has no {MANIFEST_FILE}",
            p.display()
        )));
    }
    Ok(p)
}

pub(crate) fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let name = e.file_name();
        if name == ".git" {
            continue;
        }
        let (src, dst) = (e.path(), to.join(&name));
        let ft = e.file_type()?;
        if ft.is_symlink() {
            let target = std::fs::read_link(&src)?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(target, &dst)?;
        } else if ft.is_dir() {
            copy_tree(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

pub(crate) fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)
}

/// How long a replaced (unreferenced) managed checkout is kept before a later install removes
/// it: invocations started just before an update keep their files.
pub const CHECKOUT_GRACE: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

/// Toggle the owner write bit on every directory and file below `root` (symlinks untouched).
/// Managed checkouts are read-only between builds, so nothing a plugin runs (a Python bytecode
/// cache, a log file) changes the reviewed tree by accident.
pub fn set_tree_writable(root: &Path, writable: bool) -> io::Result<()> {
    #[cfg(unix)]
    fn set(p: &Path, meta: &std::fs::Metadata, writable: bool) -> io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode() & 0o7777;
        let new = if writable {
            mode | 0o200
        } else {
            mode & !0o222
        };
        if new != mode {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(new))?;
        }
        Ok(())
    }
    #[cfg(not(unix))]
    fn set(_: &Path, _: &std::fs::Metadata, _: bool) -> io::Result<()> {
        Ok(())
    }
    let meta = std::fs::symlink_metadata(root)?;
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    // Directories are made writable before descending and read-only after.
    if meta.is_dir() {
        if writable {
            set(root, &meta, true)?;
        }
        for e in std::fs::read_dir(root)? {
            set_tree_writable(&e?.path(), writable)?;
        }
        if !writable {
            set(root, &meta, false)?;
        }
        Ok(())
    } else {
        set(root, &meta, writable)
    }
}

/// Remove a managed checkout (read-only on disk) completely.
pub(crate) fn remove_checkout(p: &Path) -> io::Result<()> {
    if std::fs::symlink_metadata(p).is_err() {
        return Ok(());
    }
    let _ = set_tree_writable(p, true);
    std::fs::remove_dir_all(p)
}

/// A unique token for checkout directory names (time, pid, counter, and kernel randomness when
/// available).
pub(crate) fn nonce() -> String {
    use std::io::Read;
    let mut b = [0u8; 8];
    let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b));
    let d = sha256_hex(format!("{}{}", new_grant_id(), hex(&b)).as_bytes());
    d[..12].to_string()
}

/// A plugin copied into the staging area and verified, not registered yet (see
/// [`Registry::install_staged`]).
#[derive(Debug)]
pub struct Staged {
    /// `<checkouts>/.staging/<label>-<nonce>`: moved into `<checkouts>/<id>/` when published.
    pub dir: PathBuf,
    pub manifest: Manifest,
    pub tree_sha256: String,
    pub origin: Origin,
}

impl Staged {
    /// Throw the staged copy away (the registration failed).
    pub fn discard(self) {
        let _ = remove_checkout(&self.dir);
    }
}

/// Copy `root` into the staging area: the copy must carry the validated manifest and contain no
/// symlink pointing outside it. No registry change, no lock needed.
pub fn stage(dirs: &PluginDirs, root: &Path, origin: Origin) -> Result<Staged, RegistryError> {
    let (m, _) = read_manifest(root)?;
    let id_dir = dirs.checkouts.join(&m.id);
    if id_dir.starts_with(root) || root.starts_with(&id_dir) || root.starts_with(&dirs.checkouts) {
        return Err(RegistryError::Conflict(
            "source and managed checkout overlap".into(),
        ));
    }
    if let Some(link) = escaping_symlink(root)? {
        return Err(RegistryError::Conflict(format!(
            "refusing to install {}: the symlink {link} points outside the plugin",
            m.id
        )));
    }
    let label = origin
        .commit
        .as_deref()
        .map(|c| c.chars().take(12).collect::<String>())
        .unwrap_or_else(|| "local".into());
    let area = dirs.checkouts.join(".staging");
    std::fs::create_dir_all(&area)?;
    let dir = area.join(format!("{label}-{}", nonce()));
    let fail = |e: RegistryError, dir: &Path| {
        let _ = remove_checkout(dir);
        Err(e)
    };
    if let Err(e) = copy_tree(root, &dir) {
        return fail(e.into(), &dir);
    }
    match read_manifest(&dir) {
        Ok((m2, _)) if m2 == m => {}
        Ok(_) => {
            return fail(
                RegistryError::Conflict("manifest changed while copying".into()),
                &dir,
            );
        }
        Err(e) => return fail(e, &dir),
    }
    // Re-checked on the copy: the source may have changed while it was read.
    match escaping_symlink(&dir) {
        Ok(None) => {}
        Ok(Some(link)) => {
            return fail(
                RegistryError::Conflict(format!(
                    "refusing to install {}: the symlink {link} points outside the plugin",
                    m.id
                )),
                &dir,
            );
        }
        Err(e) => return fail(e.into(), &dir),
    }
    let tree = match tree_digest(&dir) {
        Ok(t) => t,
        Err(e) => return fail(e.into(), &dir),
    };
    Ok(Staged {
        dir,
        manifest: m,
        tree_sha256: tree,
        origin,
    })
}

/// Stage a local plugin directory (or its manifest path).
pub fn stage_local(
    dirs: &PluginDirs,
    src: &Path,
    requested_ref: Option<&str>,
) -> Result<Staged, RegistryError> {
    let root = plugin_root(src)?;
    let origin = Origin {
        kind: "local".into(),
        path: root.clone(),
        requested_ref: requested_ref.map(str::to_string),
        repo: None,
        commit: None,
    };
    stage(dirs, &root, origin)
}

/// Stage a fetched `owner/repo` checkout ([`super::source::fetch`]): the source is the
/// repository URL, the pinned value the resolved commit.
pub fn stage_git(
    dirs: &PluginDirs,
    fetched: &super::source::Fetched,
    src: &super::source::GitSource,
) -> Result<Staged, RegistryError> {
    let root = plugin_root(&fetched.plugin_dir)?;
    let mut path = PathBuf::from(&fetched.url);
    if let Some(s) = &src.subdir {
        path = path.join(s);
    }
    let origin = Origin {
        kind: "git".into(),
        path,
        requested_ref: src.git_ref.clone(),
        repo: Some(src.spec()),
        commit: Some(fetched.commit.clone()),
    };
    stage(dirs, &root, origin)
}

/// Remove `<checkouts>/<id>/*` checkouts (and leftovers of the old flat layout) that no entry
/// references and that are older than [`CHECKOUT_GRACE`]; `keep` is never removed.
fn gc_checkouts(dirs: &PluginDirs, id: &str, keep: &[&Path]) {
    let old = |p: &Path| {
        std::fs::symlink_metadata(p)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > CHECKOUT_GRACE)
    };
    let kept = |p: &Path| keep.contains(&p);
    let id_dir = dirs.checkouts.join(id);
    if let Ok(rd) = std::fs::read_dir(&id_dir) {
        for e in rd.flatten() {
            let p = e.path();
            if !kept(&p) && old(&p) {
                let _ = remove_checkout(&p);
            }
        }
    }
    let legacy = format!(".{id}.legacy-");
    for area in [dirs.checkouts.clone(), dirs.checkouts.join(".staging")] {
        if let Ok(rd) = std::fs::read_dir(&area) {
            for e in rd.flatten() {
                let p = e.path();
                let name = e.file_name().to_string_lossy().into_owned();
                let ours = name.starts_with(&legacy) || area.ends_with(".staging");
                if ours && !kept(&p) && old(&p) {
                    let _ = remove_checkout(&p);
                }
            }
        }
    }
}

/// Before a launch of a managed checkout: the whole tree must still be the one the grant
/// recorded (the reviewed tree, or the tree after the recorded build). Cached by a stat
/// fingerprint ([`tree_digest_cached`]). Linked development directories are reviewed in place
/// and pinned by the manifest and referenced-file digests only.
pub fn verify_launch(e: &Entry) -> Result<(), String> {
    if !e.managed {
        return Ok(());
    }
    let Some(g) = &e.trust else {
        return Err(format!("{} is not trusted", e.id));
    };
    let expected = if !g.run_tree_sha256.is_empty() {
        g.run_tree_sha256.as_str()
    } else if !e.built {
        g.tree_sha256.as_str()
    } else {
        ""
    };
    if expected.is_empty() {
        return Err(format!(
            "{}'s grant records no tree digest to verify; review it again with `vibeke plugin trust {} --legacy`",
            e.id, e.id
        ));
    }
    let actual =
        tree_digest_cached(&e.root).map_err(|err| format!("{}: {err}", e.root.display()))?;
    if actual != expected {
        return Err(format!(
            "{}'s checkout {} changed since it was reviewed (whole-tree digest); not started. Reinstall or review it with `vibeke plugin trust {} --legacy`",
            e.id,
            e.root.display(),
            e.id
        ));
    }
    Ok(())
}

impl Registry {
    /// [`Registry::load`] under the shared registry lock: never observes a half-made change and
    /// waits for a running publish (install, update, trust) to finish. Launchers use it.
    pub fn load_shared(dirs: &PluginDirs) -> Result<Registry, RegistryError> {
        let _lock = RegistryLock::acquire_mode(dirs, false)?;
        Registry::load(dirs)
    }

    pub fn load(dirs: &PluginDirs) -> Result<Registry, RegistryError> {
        match std::fs::read_to_string(&dirs.registry) {
            Ok(s) => serde_json::from_str(&s).map_err(|e| RegistryError::Corrupt(e.to_string())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Registry {
                version: 1,
                ..Default::default()
            }),
            Err(e) => Err(e.into()),
        }
    }

    /// Read-modify-write under the registry's exclusive lock: load the current file, apply `f`,
    /// save when `f` succeeds. Every registry change goes through here, so concurrent processes
    /// never overwrite each other's decisions with a stale snapshot.
    pub fn update<T>(
        dirs: &PluginDirs,
        f: impl FnOnce(&mut Registry) -> Result<T, RegistryError>,
    ) -> Result<T, RegistryError> {
        let _lock = RegistryLock::acquire(dirs)?;
        let mut reg = Registry::load(dirs)?;
        let out = f(&mut reg)?;
        reg.generation += 1;
        reg.save(dirs)?;
        Ok(out)
    }

    /// Atomic replace (tmp + rename, 0600). Outside tests, changes go through
    /// [`Registry::update`].
    pub fn save(&self, dirs: &PluginDirs) -> Result<(), RegistryError> {
        let mut r = self.clone();
        r.version = 1;
        let text =
            serde_json::to_vec_pretty(&r).map_err(|e| RegistryError::Corrupt(e.to_string()))?;
        write_private(&dirs.registry, &text)?;
        // The commit-pinned lock file next to it (09 §6); best effort, never fails a save.
        let _ = crate::native::lockfile::write(dirs, &r);
        Ok(())
    }

    pub fn get(&self, id: &str) -> Result<&Entry, RegistryError> {
        self.plugins
            .get(id)
            .ok_or_else(|| RegistryError::NotFound(id.to_string()))
    }

    /// Copy a local plugin directory into a managed checkout and register it (untrusted).
    /// Reinstalling a managed plugin publishes a new checkout; a link is never replaced
    /// silently. (CLI installs stage outside the registry lock: [`stage_local`] and
    /// [`Registry::install_staged`].)
    pub fn install(
        &mut self,
        dirs: &PluginDirs,
        src: &Path,
        requested_ref: Option<&str>,
    ) -> Result<(Entry, Manifest), RegistryError> {
        let staged = stage_local(dirs, src, requested_ref)?;
        self.install_staged(dirs, staged)
    }

    /// Register a fetched `owner/repo` checkout as a managed install ([`stage_git`]).
    pub fn install_git(
        &mut self,
        dirs: &PluginDirs,
        fetched: &super::source::Fetched,
        src: &super::source::GitSource,
    ) -> Result<(Entry, Manifest), RegistryError> {
        let staged = stage_git(dirs, fetched, src)?;
        self.install_staged(dirs, staged)
    }

    /// Publish a staged checkout: move it to its immutable path `<checkouts>/<id>/<dir>` and
    /// point the entry (root, origin, kept or dropped grant) at it. Nothing on disk that the
    /// current registry references is touched, so until the caller saves the registry (the end
    /// of [`Registry::update`]) the old checkout stays in use; a failed save or a crash leaves
    /// an unreferenced directory that a later install removes. The staged copy is discarded on
    /// error.
    pub fn install_staged(
        &mut self,
        dirs: &PluginDirs,
        staged: Staged,
    ) -> Result<(Entry, Manifest), RegistryError> {
        let m = staged.manifest.clone();
        if self.native.contains_key(&m.id) {
            let msg = format!("{} is registered as a native plugin; remove it first", m.id);
            staged.discard();
            return Err(RegistryError::Conflict(msg));
        }
        if let Some(old) = self.plugins.get(&m.id)
            && !old.managed
        {
            let msg = format!(
                "{} is linked from {}; unlink it before installing",
                m.id,
                old.root.display()
            );
            staged.discard();
            return Err(RegistryError::Conflict(msg));
        }
        let id_dir = dirs.checkouts.join(&m.id);
        let name = staged
            .dir
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_default();
        let dest = id_dir.join(&name);
        let prev = self.plugins.get(&m.id).cloned();
        // The old flat layout (`<checkouts>/<id>` holding the files) is moved aside first: new
        // checkouts live below `<checkouts>/<id>/`. If this is the registered root, a crash
        // before the save leaves it `broken` (never stale content running).
        if manifest_file(&id_dir).is_file() {
            let aside = dirs.checkouts.join(format!(".{}.legacy-{}", m.id, nonce()));
            if let Err(e) = std::fs::rename(&id_dir, &aside) {
                staged.discard();
                return Err(e.into());
            }
        }
        // Read-only once in place (moving a directory needs it writable).
        let moved = std::fs::create_dir_all(&id_dir)
            .and_then(|_| std::fs::rename(&staged.dir, &dest))
            .and_then(|_| set_tree_writable(&dest, false));
        if let Err(e) = moved {
            let _ = remove_checkout(&dest);
            staged.discard();
            return Err(e.into());
        }
        let tree = staged.tree_sha256;
        let origin = staged.origin;
        // The grant survives a reinstall only for the exact reviewed content from the same
        // source, and only when there is nothing to rebuild (the fresh checkout is unbuilt).
        let needs_build = !m.build_on(super::current_platform()).is_empty();
        let trust = prev
            .as_ref()
            .and_then(|p| p.trust.clone())
            .filter(|g| {
                !needs_build
                    && g.source == origin.path
                    && g.commit == origin.commit
                    && !g.tree_sha256.is_empty()
                    && g.tree_sha256 == tree
            })
            .map(|mut g| {
                g.root = dest.clone();
                g.run_tree_sha256 = tree.clone();
                g
            });
        let mut keep: Vec<&Path> = vec![dest.as_path()];
        if let Some(p) = &prev {
            keep.push(p.root.as_path());
        }
        gc_checkouts(dirs, &m.id, &keep);
        let entry = Entry {
            id: m.id.clone(),
            kind: "herdr".into(),
            root: dest,
            managed: true,
            origin,
            enabled: prev.as_ref().is_none_or(|p| p.enabled),
            built: false,
            installed_at_ms: now_ms(),
            trust,
        };
        self.plugins.insert(m.id.clone(), entry.clone());
        Ok((entry, m))
    }

    /// Register a directory in place (development link). Never builds.
    pub fn link(&mut self, path: &Path) -> Result<(Entry, Manifest), RegistryError> {
        let root = plugin_root(path)?;
        let (m, _) = read_manifest(&root)?;
        if self.native.contains_key(&m.id) {
            return Err(RegistryError::Conflict(format!(
                "{} is registered as a native plugin; remove it first",
                m.id
            )));
        }
        if let Some(old) = self.plugins.get(&m.id) {
            if old.managed {
                return Err(RegistryError::Conflict(format!(
                    "{} is installed; uninstall it before linking",
                    m.id
                )));
            }
            if old.root != root {
                return Err(RegistryError::Conflict(format!(
                    "{} is already linked from {}",
                    m.id,
                    old.root.display()
                )));
            }
        }
        let prev = self.plugins.get(&m.id).cloned();
        let entry = Entry {
            id: m.id.clone(),
            kind: "herdr".into(),
            root: root.clone(),
            managed: false,
            origin: Origin {
                kind: "link".into(),
                path: root,
                requested_ref: None,
                repo: None,
                commit: None,
            },
            enabled: prev.as_ref().is_none_or(|p| p.enabled),
            built: false,
            installed_at_ms: prev.as_ref().map_or_else(now_ms, |p| p.installed_at_ms),
            trust: prev.and_then(|p| p.trust),
        };
        self.plugins.insert(m.id.clone(), entry.clone());
        Ok((entry, m))
    }

    /// Remove a link registration (its files stay) and its grant.
    pub fn unlink(&mut self, id: &str) -> Result<Entry, RegistryError> {
        let e = self.get(id)?.clone();
        if e.managed {
            return Err(RegistryError::Conflict(format!(
                "{id} is installed, not linked; use uninstall"
            )));
        }
        self.plugins.remove(id);
        Ok(e)
    }

    /// Remove a managed install: the checkout is deleted, plugin config/state are preserved.
    pub fn uninstall(&mut self, dirs: &PluginDirs, id: &str) -> Result<Entry, RegistryError> {
        let e = self.get(id)?.clone();
        if !e.managed {
            return Err(RegistryError::Conflict(format!(
                "{id} is linked; use unlink (files are kept)"
            )));
        }
        if e.root.starts_with(&dirs.checkouts) {
            let _ = remove_checkout(&e.root);
            // Every other checkout of the plugin (replaced ones, the old flat layout).
            let id_dir = dirs.checkouts.join(id);
            if e.root.starts_with(&id_dir) {
                let _ = remove_checkout(&id_dir);
            }
        }
        self.plugins.remove(id);
        Ok(e)
    }

    pub fn set_enabled(&mut self, id: &str, on: bool) -> Result<Entry, RegistryError> {
        let e = self
            .plugins
            .get_mut(id)
            .ok_or_else(|| RegistryError::NotFound(id.to_string()))?;
        e.enabled = on;
        Ok(e.clone())
    }

    /// Record the explicit `herdr_legacy` grant for the manifest currently on disk.
    pub fn trust(&mut self, id: &str) -> Result<Grant, RegistryError> {
        let e = self
            .plugins
            .get_mut(id)
            .ok_or_else(|| RegistryError::NotFound(id.to_string()))?;
        let (m, digest) = read_manifest(&e.root)?;
        if m.id != e.id {
            return Err(RegistryError::Conflict(format!(
                "manifest id changed to {}; reinstall",
                m.id
            )));
        }
        if let Some(why) = escaping_entrypoint(&e.root, &m) {
            return Err(RegistryError::Conflict(format!(
                "{id}: entrypoint {why}, outside the plugin root; it cannot be reviewed"
            )));
        }
        let tree = tree_digest(&e.root)?;
        let g = Grant {
            mode: "herdr_legacy".into(),
            manifest_sha256: digest,
            root: e.root.clone(),
            granted_at_ms: now_ms(),
            entrypoints: m.entrypoints(super::current_platform()),
            baseline: BASELINE_VERSION.into(),
            grant_id: new_grant_id(),
            source: e.origin.path.clone(),
            tree_sha256: tree.clone(),
            entry_sha256: entry_digest(&e.root, &m),
            commit: e.origin.commit.clone(),
            run_tree_sha256: tree.clone(),
        };
        // A new review of a managed checkout requires a new build.
        if e.managed {
            e.built = false;
        }
        e.trust = Some(g.clone());
        Ok(g)
    }

    /// Record a successful build for the grant `grant_id`: only if that grant is still the
    /// current one and the manifest is still the reviewed one. The referenced files' digest is
    /// re-recorded (the build may have produced them).
    pub fn finish_build(&mut self, id: &str, grant_id: &str) -> Result<(), RegistryError> {
        let e = self
            .plugins
            .get_mut(id)
            .ok_or_else(|| RegistryError::NotFound(id.to_string()))?;
        let (m, digest) = read_manifest(&e.root)?;
        let Some(g) = e.trust.as_mut().filter(|g| g.grant_id == grant_id) else {
            return Err(RegistryError::Conflict(format!(
                "the grant for {id} changed during the build (revoked or re-reviewed)"
            )));
        };
        if g.manifest_sha256 != digest {
            return Err(RegistryError::Conflict(format!(
                "{id}'s manifest changed during the build"
            )));
        }
        if let Some(why) = escaping_entrypoint(&e.root, &m) {
            return Err(RegistryError::Conflict(format!(
                "{id}: the build left entrypoint {why}, outside the plugin root"
            )));
        }
        g.entry_sha256 = entry_digest(&e.root, &m);
        if e.managed {
            g.run_tree_sha256 = tree_digest(&e.root)?;
        }
        e.built = true;
        Ok(())
    }

    pub fn revoke(&mut self, id: &str) -> Result<Entry, RegistryError> {
        let e = self
            .plugins
            .get_mut(id)
            .ok_or_else(|| RegistryError::NotFound(id.to_string()))?;
        e.trust = None;
        Ok(e.clone())
    }

    /// Effective status, re-reading the manifest so a changed file disables execution.
    pub fn status(&self, id: &str) -> Result<(Status, Option<Manifest>), RegistryError> {
        let e = self.get(id)?;
        Ok(entry_status(e))
    }

    /// Active plugins with their manifests (what the server may run).
    pub fn active(&self) -> Vec<(Entry, Manifest)> {
        self.plugins
            .values()
            .filter_map(|e| match entry_status(e) {
                (Status::Active, Some(m)) => Some((e.clone(), m)),
                _ => None,
            })
            .collect()
    }
}

/// Status of one entry.
pub fn entry_status(e: &Entry) -> (Status, Option<Manifest>) {
    let Ok((m, digest)) = read_manifest(&e.root) else {
        return (Status::Broken, None);
    };
    if escaping_entrypoint(&e.root, &m).is_some() {
        return (Status::Broken, Some(m));
    }
    let unbuilt = e.managed && !e.built && !m.build_on(super::current_platform()).is_empty();
    let status = match &e.trust {
        None => Status::Untrusted,
        Some(g)
            if g.manifest_sha256 != digest
                || g.root != e.root
                || m.id != e.id
                || g.source != e.origin.path
                || g.commit != e.origin.commit
                || g.grant_id.is_empty()
                || g.entry_sha256 != entry_digest(&e.root, &m)
                || unbuilt =>
        {
            Status::StaleTrust
        }
        Some(_) if !e.enabled => Status::Disabled,
        Some(_) => Status::Active,
    };
    (status, Some(m))
}

/// The legacy trust terms shown before a grant (07 §7.7 "Trust and revocation", 09 §6).
pub fn trust_terms(e: &Entry, m: &Manifest, digest: &str) -> String {
    let pf = super::current_platform();
    let mut s = String::new();
    s.push_str(&format!(
        "Herdr legacy trust for {} {} ({})\n",
        m.id,
        m.version.as_deref().unwrap_or("?"),
        m.name.as_deref().unwrap_or(&m.id)
    ));
    s.push_str(&format!(
        "  source:   {} ({}{})\n",
        e.origin.path.display(),
        e.origin.kind,
        e.origin
            .requested_ref
            .as_deref()
            .map(|r| format!(", ref {r}"))
            .unwrap_or_default()
    ));
    if let Some(c) = &e.origin.commit {
        s.push_str(&format!("  commit:   {c}\n"));
    }
    s.push_str(&format!("  root:     {}\n", e.root.display()));
    s.push_str(&format!("  manifest: sha256 {digest}\n"));
    if let Some(v) = &m.min_herdr_version {
        s.push_str(&format!(
            "  requires: Herdr {v} (emulated baseline {BASELINE_VERSION})\n"
        ));
    }
    let list = |title: &str, items: Vec<String>, s: &mut String| {
        if !items.is_empty() {
            s.push_str(&format!("  {title}:\n"));
            for i in items {
                s.push_str(&format!("    - {i}\n"));
            }
        }
    };
    list(
        "build (runs once after trust, without socket access)",
        m.build_on(pf).iter().map(|b| b.command.join(" ")).collect(),
        &mut s,
    );
    list(
        "startup (runs once per server start)",
        m.startup_on(pf)
            .iter()
            .map(|b| b.command.join(" "))
            .collect(),
        &mut s,
    );
    list(
        "actions",
        m.actions_on(pf)
            .iter()
            .map(|a| {
                format!(
                    "{} [{}] — {}: {}",
                    a.id,
                    a.contexts.join(","),
                    a.title,
                    a.command.join(" ")
                )
            })
            .collect(),
        &mut s,
    );
    list(
        "event hooks",
        m.events
            .iter()
            .filter(|e| m.entry_on(e.platforms.as_ref(), pf))
            .map(|e| format!("{}: {}", e.on, e.command.join(" ")))
            .collect(),
        &mut s,
    );
    list(
        "panes",
        m.panes
            .iter()
            .filter(|p| m.entry_on(p.platforms.as_ref(), pf))
            .map(|p| format!("{} ({}): {}", p.id, p.placement, p.command.join(" ")))
            .collect(),
        &mut s,
    );
    list(
        "link handlers",
        m.link_handlers
            .iter()
            .map(|l| format!("{} {} → {}", l.id, l.pattern, l.action))
            .collect(),
        &mut s,
    );
    s.push_str(
        "\nHerdr plugins declare no capabilities. A legacy grant lets this plugin's programs run\n\
         as you, with your environment, filesystem and network, and call the complete Herdr\n\
         compatibility API (workspaces, panes, input, agents, notifications) of this Vibeke\n\
         session. It does not grant native-only administrative APIs, holder keys or other\n\
         plugins' identities. Any change to the manifest, to a file its commands reference or\n\
         (on reinstall) to the installed content requires a new review and, when the plugin\n\
         declares [[build]], a new build.\n",
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temp dir that can be removed although managed checkouts in it are read-only.
    struct Tmp(tempfile::TempDir);

    impl Tmp {
        fn path(&self) -> &Path {
            self.0.path()
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = set_tree_writable(self.0.path(), true);
        }
    }

    fn tmp() -> Tmp {
        Tmp(tempfile::tempdir().unwrap())
    }

    fn dirs(t: &Path) -> PluginDirs {
        PluginDirs {
            registry: t.join("cfg/plugins.json"),
            checkouts: t.join("state/plugins/checkouts"),
            config: t.join("cfg/plugins"),
            state: t.join("state/plugins/state"),
        }
    }

    fn plugin(dir: &Path, id: &str, extra: &str) -> PathBuf {
        let p = dir.join(id);
        std::fs::create_dir_all(p.join("bin")).unwrap();
        std::fs::write(
            p.join(MANIFEST_FILE),
            format!(
                "id = \"{id}\"\nversion = \"1.0.0\"\n[[actions]]\nid = \"go\"\ntitle = \"Go\"\ncommand = [\"bin/go\"]\n{extra}"
            ),
        )
        .unwrap();
        std::fs::write(p.join("bin/go"), "#!/bin/sh\necho go\n").unwrap();
        std::fs::create_dir_all(p.join(".git")).unwrap();
        p
    }

    #[test]
    fn install_is_untrusted_until_granted_and_digest_bound() {
        let t = tmp();
        let d = dirs(t.path());
        let src = plugin(t.path(), "acme.one", "");
        let mut r = Registry::load(&d).unwrap();
        let (e, m) = r.install(&d, &src, None).unwrap();
        assert!(e.managed && e.enabled && e.trust.is_none());
        assert!(e.root.join("bin/go").is_file());
        assert!(!e.root.join(".git").exists(), ".git is not copied");
        assert_eq!(m.actions.len(), 1);
        assert_eq!(r.status("acme.one").unwrap().0, Status::Untrusted);
        assert!(r.active().is_empty(), "nothing runs before trust");
        r.save(&d).unwrap();

        let mut r = Registry::load(&d).unwrap();
        let g = r.trust("acme.one").unwrap();
        assert_eq!(g.mode, "herdr_legacy");
        assert!(g.entrypoints.iter().any(|x| x.contains("action go")));
        assert_eq!(r.status("acme.one").unwrap().0, Status::Active);
        assert_eq!(r.active().len(), 1);

        r.set_enabled("acme.one", false).unwrap();
        assert_eq!(r.status("acme.one").unwrap().0, Status::Disabled);
        r.set_enabled("acme.one", true).unwrap();

        // The managed checkout is read-only on disk.
        let root = r.get("acme.one").unwrap().root.clone();
        assert!(std::fs::write(root.join("bin/go"), "x").is_err());
        assert!(std::fs::write(root.join("new"), "x").is_err());
        // Editing the managed manifest (after forcing it writable) invalidates the grant.
        set_tree_writable(&root, true).unwrap();
        let mf = r.get("acme.one").unwrap().root.join(MANIFEST_FILE);
        let text = std::fs::read_to_string(&mf).unwrap();
        std::fs::write(
            &mf,
            format!("{text}\n[[startup]]\ncommand = [\"bin/go\"]\n"),
        )
        .unwrap();
        assert_eq!(r.status("acme.one").unwrap().0, Status::StaleTrust);
        assert!(r.active().is_empty());

        // Reinstalling the original source restores the reviewed digest.
        r.install(&d, &src, None).unwrap();
        assert_eq!(r.status("acme.one").unwrap().0, Status::Active);

        r.revoke("acme.one").unwrap();
        assert_eq!(r.status("acme.one").unwrap().0, Status::Untrusted);

        let e = r.uninstall(&d, "acme.one").unwrap();
        assert!(!e.root.exists(), "managed checkout removed");
        assert!(src.join(MANIFEST_FILE).exists(), "source untouched");
        assert!(r.get("acme.one").is_err());
    }

    #[test]
    fn links_and_conflicts() {
        let t = tmp();
        let d = dirs(t.path());
        let src = plugin(t.path(), "acme.two", "");
        let mut r = Registry::default();
        // A direct manifest path is accepted.
        let (e, _) = r.link(&src.join(MANIFEST_FILE)).unwrap();
        assert!(!e.managed);
        assert_eq!(e.root, std::fs::canonicalize(&src).unwrap());
        assert!(matches!(
            r.install(&d, &src, None),
            Err(RegistryError::Conflict(_))
        ));
        assert!(matches!(
            r.uninstall(&d, "acme.two"),
            Err(RegistryError::Conflict(_))
        ));
        r.trust("acme.two").unwrap();
        r.unlink("acme.two").unwrap();
        assert!(src.join("bin/go").exists(), "unlink keeps files");
        // Relinking starts without the old grant.
        r.link(&src).unwrap();
        assert_eq!(r.status("acme.two").unwrap().0, Status::Untrusted);
        assert!(matches!(r.unlink("nope"), Err(RegistryError::NotFound(_))));
    }

    #[test]
    fn broken_and_invalid_manifests() {
        let t = tmp();
        let d = dirs(t.path());
        let src = plugin(t.path(), "acme.three", "");
        let mut r = Registry::default();
        r.link(&src).unwrap();
        r.trust("acme.three").unwrap();
        std::fs::remove_file(src.join(MANIFEST_FILE)).unwrap();
        assert_eq!(r.status("acme.three").unwrap().0, Status::Broken);
        let bad = t.path().join("bad");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(
            bad.join(MANIFEST_FILE),
            "id = 'x'\nmin_herdr_version = '9.0.0'",
        )
        .unwrap();
        assert!(matches!(
            r.install(&d, &bad, None),
            Err(RegistryError::Manifest(ManifestError::VersionTooNew { .. }))
        ));
        assert!(r.install(&d, &t.path().join("missing"), None).is_err());
    }

    #[test]
    fn reinstalling_changed_content_with_the_same_manifest_needs_review() {
        let t = tmp();
        let d = dirs(t.path());
        let src = plugin(t.path(), "acme.five", "");
        let mut r = Registry::default();
        r.install(&d, &src, None).unwrap();
        r.trust("acme.five").unwrap();
        assert_eq!(r.status("acme.five").unwrap().0, Status::Active);
        // Same manifest bytes, different script: the reinstall is inactive.
        std::fs::write(src.join("bin/go"), "#!/bin/sh\necho evil\n").unwrap();
        let (e, _) = r.install(&d, &src, None).unwrap();
        assert!(e.trust.is_none(), "grant dropped");
        assert_eq!(r.status("acme.five").unwrap().0, Status::Untrusted);
        // An unreferenced extra file changes the tree too.
        r.trust("acme.five").unwrap();
        std::fs::write(src.join("lib.sh"), "x").unwrap();
        r.install(&d, &src, None).unwrap();
        assert_eq!(r.status("acme.five").unwrap().0, Status::Untrusted);
        // The same content from a different source directory is a different registration.
        r.trust("acme.five").unwrap();
        let other = t.path().join("copy");
        copy_tree(&src, &other).unwrap();
        r.install(&d, &other, None).unwrap();
        assert_eq!(r.status("acme.five").unwrap().0, Status::Untrusted);
        // Editing a referenced file in place makes the grant stale.
        r.trust("acme.five").unwrap();
        let root = r.get("acme.five").unwrap().root.clone();
        set_tree_writable(&root, true).unwrap();
        std::fs::write(root.join("bin/go"), "#!/bin/sh\necho changed\n").unwrap();
        assert_eq!(r.status("acme.five").unwrap().0, Status::StaleTrust);
    }

    #[test]
    fn plugins_with_a_build_need_a_build_for_each_grant() {
        let t = tmp();
        let d = dirs(t.path());
        let src = plugin(
            t.path(),
            "acme.six",
            "[[build]]\ncommand = [\"sh\", \"-c\", \"true\"]\n",
        );
        let mut r = Registry::default();
        r.install(&d, &src, None).unwrap();
        let g = r.trust("acme.six").unwrap();
        assert_eq!(
            r.status("acme.six").unwrap().0,
            Status::StaleTrust,
            "trusted but not built yet"
        );
        r.finish_build("acme.six", &g.grant_id).unwrap();
        assert_eq!(r.status("acme.six").unwrap().0, Status::Active);
        // An identical reinstall still needs review + build (the fresh checkout is unbuilt).
        r.install(&d, &src, None).unwrap();
        assert_eq!(r.status("acme.six").unwrap().0, Status::Untrusted);
        // A build for a grant that was revoked or replaced is not recorded.
        let g1 = r.trust("acme.six").unwrap();
        r.revoke("acme.six").unwrap();
        let _g2 = r.trust("acme.six").unwrap();
        assert!(matches!(
            r.finish_build("acme.six", &g1.grant_id),
            Err(RegistryError::Conflict(_))
        ));
        assert_eq!(r.status("acme.six").unwrap().0, Status::StaleTrust);
    }

    #[test]
    fn concurrent_updates_never_lose_a_revocation() {
        let t = tmp();
        let d = dirs(t.path());
        let a = plugin(t.path(), "acme.a", "");
        let b = plugin(t.path(), "acme.b", "");
        Registry::update(&d, |r| {
            r.link(&a)?;
            r.link(&b)?;
            r.trust("acme.a")?;
            r.trust("acme.b")?;
            Ok(())
        })
        .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let d2 = d.clone();
        let slow = std::thread::spawn(move || {
            Registry::update(&d2, |r| {
                tx.send(()).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(300));
                r.set_enabled("acme.b", false)
            })
            .unwrap();
        });
        rx.recv().unwrap();
        Registry::update(&d, |r| r.revoke("acme.a")).unwrap();
        slow.join().unwrap();
        let r = Registry::load(&d).unwrap();
        assert!(r.get("acme.a").unwrap().trust.is_none(), "revocation kept");
        assert!(!r.get("acme.b").unwrap().enabled, "other change kept");
    }

    /// A plugin whose unchanged entrypoint `bin/main.sh` runs `lib/helper.sh`.
    fn dep_plugin(dir: &Path, helper: &str) -> PathBuf {
        let p = dir.join("acme.dep");
        std::fs::create_dir_all(p.join("bin")).unwrap();
        std::fs::create_dir_all(p.join("lib")).unwrap();
        std::fs::write(
            p.join(MANIFEST_FILE),
            "id = \"acme.dep\"\n[[actions]]\nid = \"go\"\ntitle = \"Go\"\ncommand = [\"sh\", \"bin/main.sh\"]\n",
        )
        .unwrap();
        std::fs::write(
            p.join("bin/main.sh"),
            ". \"$(dirname \"$0\")/../lib/helper.sh\"\n",
        )
        .unwrap();
        std::fs::write(p.join("lib/helper.sh"), helper).unwrap();
        p
    }

    #[test]
    fn a_dependency_only_update_is_published_atomically_with_the_revoked_grant() {
        let t = tmp();
        let d = dirs(t.path());
        let src = dep_plugin(t.path(), "echo reviewed\n");
        Registry::update(&d, |r| {
            r.install(&d, &src, None)?;
            r.trust("acme.dep")?;
            Ok(())
        })
        .unwrap();
        let before = Registry::load(&d).unwrap().get("acme.dep").unwrap().clone();
        assert_eq!(entry_status(&before).0, Status::Active);
        assert!(verify_launch(&before).is_ok());
        let id_dir = d.checkouts.join("acme.dep");
        assert!(before.root.starts_with(&id_dir) && before.root != id_dir);

        // Only the helper changes upstream: the manifest and the entrypoint are identical.
        std::fs::write(src.join("lib/helper.sh"), "echo injected\n").unwrap();

        // Failure immediately before the registry save (a crash, a full disk): the registry
        // still names the old checkout, which still holds the reviewed helper.
        let r = Registry::update(&d, |r| {
            r.install(&d, &src, None)?;
            Err::<(), _>(RegistryError::Conflict(
                "simulated crash before save".into(),
            ))
        });
        assert!(r.is_err());
        let now = Registry::load(&d).unwrap().get("acme.dep").unwrap().clone();
        assert_eq!(now, before, "nothing was published");
        assert_eq!(entry_status(&now).0, Status::Active);
        assert!(verify_launch(&now).is_ok());
        assert_eq!(
            std::fs::read_to_string(now.root.join("lib/helper.sh")).unwrap(),
            "echo reviewed\n",
            "the old checkout is never modified"
        );

        // A real update. A launcher that resolved the entry before the switch keeps running the
        // old, unmodified checkout; a launcher reading under the shared lock waits for the save
        // and sees the new root with the grant revoked in the same write.
        let (tx, rx) = std::sync::mpsc::channel();
        let (d2, src2) = (d.clone(), src.clone());
        let writer = std::thread::spawn(move || {
            Registry::update(&d2, |r| {
                r.install(&d2, &src2, None)?;
                tx.send(()).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(300));
                Ok(())
            })
            .unwrap();
        });
        rx.recv().unwrap();
        let seen = Registry::load_shared(&d).unwrap();
        writer.join().unwrap();
        let after = seen.get("acme.dep").unwrap();
        assert_ne!(after.root, before.root, "published at a new immutable path");
        assert!(
            after.trust.is_none(),
            "the changed tree is not covered by the grant"
        );
        assert_eq!(entry_status(after).0, Status::Untrusted);
        assert_eq!(
            std::fs::read_to_string(after.root.join("lib/helper.sh")).unwrap(),
            "echo injected\n"
        );
        assert_eq!(
            std::fs::read_to_string(before.root.join("lib/helper.sh")).unwrap(),
            "echo reviewed\n",
            "a stale launcher still finds exactly the reviewed content"
        );
        assert!(verify_launch(&before).is_ok());
        // Both checkouts exist until a later install collects the old one after the grace.
        let n = std::fs::read_dir(&id_dir).unwrap().count();
        assert!(n >= 2, "{n}");
        // Uninstall removes every checkout of the plugin.
        Registry::update(&d, |r| r.uninstall(&d, "acme.dep")).unwrap();
        assert!(!id_dir.exists());
    }

    #[test]
    fn a_launch_verifies_the_whole_tree() {
        let t = tmp();
        let d = dirs(t.path());
        let src = dep_plugin(t.path(), "echo reviewed\n");
        let mut r = Registry::default();
        r.install(&d, &src, None).unwrap();
        r.trust("acme.dep").unwrap();
        let e = r.get("acme.dep").unwrap().clone();
        assert!(verify_launch(&e).is_ok());
        assert!(verify_launch(&e).is_ok(), "cached");
        // A dependency edited in place (forcing the read-only checkout writable): the manifest
        // and the referenced entrypoint are unchanged, so the status stays active, but no
        // launch happens.
        set_tree_writable(&e.root, true).unwrap();
        std::fs::write(e.root.join("lib/helper.sh"), "echo injected\n").unwrap();
        assert_eq!(entry_status(&e).0, Status::Active);
        let err = verify_launch(&e).unwrap_err();
        assert!(err.contains("changed since it was reviewed"), "{err}");
        // A new file is a change too; restoring the content makes it launchable again.
        std::fs::write(e.root.join("lib/helper.sh"), "echo reviewed\n").unwrap();
        assert!(verify_launch(&e).is_ok());
        std::fs::write(e.root.join("lib/extra.sh"), "x").unwrap();
        assert!(verify_launch(&e).is_err());
        std::fs::remove_file(e.root.join("lib/extra.sh")).unwrap();
        assert!(verify_launch(&e).is_ok());
        // A build re-records the tree it leaves behind.
        let src_b = plugin(
            t.path(),
            "acme.built",
            "[[build]]\ncommand = [\"sh\", \"-c\", \"true\"]\n",
        );
        r.install(&d, &src_b, None).unwrap();
        let g = r.trust("acme.built").unwrap();
        let root = r.get("acme.built").unwrap().root.clone();
        set_tree_writable(&root, true).unwrap();
        std::fs::write(root.join("dist.js"), "built").unwrap();
        set_tree_writable(&root, false).unwrap();
        r.finish_build("acme.built", &g.grant_id).unwrap();
        assert!(verify_launch(r.get("acme.built").unwrap()).is_ok());
        // Linked development directories are reviewed in place: not tree-verified.
        let l = plugin(t.path(), "acme.linked", "");
        r.link(&l).unwrap();
        r.trust("acme.linked").unwrap();
        std::fs::write(l.join("notes.txt"), "edit").unwrap();
        assert!(verify_launch(r.get("acme.linked").unwrap()).is_ok());
    }

    #[test]
    fn symlinks_are_pinned_by_their_targets_and_escapes_are_refused() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let t = tmp();
            let d = dirs(t.path());
            let outside = t.path().join("outside.sh");
            std::fs::write(&outside, "echo outside\n").unwrap();

            // An internal link entrypoint: the target's contents are pinned.
            let p = t.path().join("acme.lnk");
            std::fs::create_dir_all(&p).unwrap();
            std::fs::write(
                p.join(MANIFEST_FILE),
                "id = \"acme.lnk\"\n[[actions]]\nid = \"go\"\ntitle = \"Go\"\ncommand = [\"sh\", \"run.sh\"]\n",
            )
            .unwrap();
            std::fs::write(p.join("impl.sh"), "echo reviewed\n").unwrap();
            symlink("impl.sh", p.join("run.sh")).unwrap();
            let mut r = Registry::default();
            r.install(&d, &p, None).unwrap();
            r.trust("acme.lnk").unwrap();
            assert_eq!(r.status("acme.lnk").unwrap().0, Status::Active);
            let root = r.get("acme.lnk").unwrap().root.clone();
            set_tree_writable(&root, true).unwrap();
            std::fs::write(root.join("impl.sh"), "echo changed\n").unwrap();
            assert_eq!(
                r.status("acme.lnk").unwrap().0,
                Status::StaleTrust,
                "changing the link target invalidates the grant"
            );

            // An install with an entrypoint linking outside the checkout is refused, and so is
            // any other escaping symlink (absolute or climbing out).
            for (name, target) in [
                ("run.sh", outside.to_string_lossy().into_owned()),
                ("data", "/etc".to_string()),
                ("up", "../../outside.sh".to_string()),
            ] {
                let q = t.path().join(format!("esc-{name}"));
                std::fs::create_dir_all(&q).unwrap();
                std::fs::write(
                    q.join(MANIFEST_FILE),
                    "id = \"acme.esc\"\n[[actions]]\nid = \"go\"\ntitle = \"Go\"\ncommand = [\"sh\", \"run.sh\"]\n",
                )
                .unwrap();
                if name != "run.sh" {
                    std::fs::write(q.join("run.sh"), "true\n").unwrap();
                }
                symlink(&target, q.join(name)).unwrap();
                let err = r.install(&d, &q, None).unwrap_err();
                assert!(
                    err.to_string().contains("points outside the plugin"),
                    "{name}: {err}"
                );
                assert!(r.get("acme.esc").is_err());
            }
            let staging = d.checkouts.join(".staging");
            assert_eq!(
                std::fs::read_dir(&staging).map(|r| r.count()).unwrap_or(0),
                0,
                "refused copies are removed"
            );

            // A linked directory is not copied, so its escaping entrypoint (a link, or a
            // directory link along the path) cannot be trusted and does not run.
            let l = t.path().join("acme.lk");
            std::fs::create_dir_all(&l).unwrap();
            std::fs::write(
                l.join(MANIFEST_FILE),
                "id = \"acme.lk\"\n[[actions]]\nid = \"go\"\ntitle = \"Go\"\ncommand = [\"sh\", \"bin/run.sh\"]\n",
            )
            .unwrap();
            let ob = t.path().join("outside-bin");
            std::fs::create_dir_all(&ob).unwrap();
            std::fs::write(ob.join("run.sh"), "echo outside\n").unwrap();
            symlink(&ob, l.join("bin")).unwrap();
            r.link(&l).unwrap();
            let err = r.trust("acme.lk").unwrap_err();
            assert!(err.to_string().contains("outside the plugin root"), "{err}");
            assert_eq!(r.status("acme.lk").unwrap().0, Status::Broken);
            // Granted before the link was swapped in: broken from then on.
            std::fs::remove_file(l.join("bin")).unwrap();
            std::fs::create_dir_all(l.join("bin")).unwrap();
            std::fs::write(l.join("bin/run.sh"), "true\n").unwrap();
            r.trust("acme.lk").unwrap();
            assert_eq!(r.status("acme.lk").unwrap().0, Status::Active);
            std::fs::remove_file(l.join("bin/run.sh")).unwrap();
            symlink(&outside, l.join("bin/run.sh")).unwrap();
            assert_eq!(r.status("acme.lk").unwrap().0, Status::Broken);
            assert!(r.active().is_empty());
        }
    }

    #[test]
    fn trust_terms_list_entrypoints() {
        let t = tmp();
        let src = plugin(
            t.path(),
            "acme.four",
            "[[events]]\non = \"worktree.created\"\ncommand = [\"bin/go\", \"hook\"]\n",
        );
        let mut r = Registry::default();
        let (e, m) = r.link(&src).unwrap();
        let (_, digest) = read_manifest(&e.root).unwrap();
        let s = trust_terms(&e, &m, &digest);
        assert!(s.contains("Herdr legacy trust for acme.four"));
        assert!(s.contains("go [global]"));
        assert!(s.contains("worktree.created: bin/go hook"));
        assert!(s.contains(&digest));
    }

    fn run_git(dir: &Path, args: &[&str]) -> String {
        let o = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(
            o.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }

    #[test]
    fn repository_installs_pin_the_commit_and_updates_need_review() {
        use super::super::source;
        let t = tmp();
        let d = dirs(t.path());
        // A repository `<srv>/acme/tool` whose plugin id is acme.tool.
        let work = t.path().join("w");
        let body = |v: &str| {
            format!(
                "id = \"acme.tool\"\nversion = \"{v}\"\n[[actions]]\nid = \"go\"\ntitle = \"Go\"\ncommand = [\"bin/go\"]\n"
            )
        };
        std::fs::create_dir_all(work.join("bin")).unwrap();
        std::fs::write(work.join(MANIFEST_FILE), body("1")).unwrap();
        std::fs::write(work.join("bin/go"), "#!/bin/sh\necho 1\n").unwrap();
        run_git(&work, &["init", "-q", "-b", "main"]);
        run_git(&work, &["add", "."]);
        run_git(&work, &["commit", "-qm", "one"]);
        run_git(&work, &["tag", "v1"]);
        let c1 = run_git(&work, &["rev-parse", "HEAD"]);
        let bare = t.path().join("srv/acme/tool");
        std::fs::create_dir_all(bare.parent().unwrap()).unwrap();
        run_git(
            t.path(),
            &[
                "clone",
                "-q",
                "--bare",
                work.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );
        let base = format!("file://{}", t.path().join("srv").display());
        let install = |reg: &mut Registry, r: Option<&str>| {
            let src = source::parse("acme/tool", r).unwrap().unwrap();
            let f = source::fetch(&src, &base, &t.path().join("fetch")).unwrap();
            let out = reg.install_git(&d, &f, &src);
            std::fs::remove_dir_all(&f.work).unwrap();
            out.unwrap()
        };
        let mut reg = Registry::default();
        let (e, _) = install(&mut reg, Some("v1"));
        assert_eq!(e.origin.kind, "git");
        assert_eq!(e.origin.commit.as_deref(), Some(c1.as_str()));
        assert_eq!(e.origin.requested_ref.as_deref(), Some("v1"));
        assert_eq!(e.origin.repo.as_deref(), Some("acme/tool"));
        assert!(!e.root.join(".git").exists());
        let g = reg.trust("acme.tool").unwrap();
        assert_eq!(
            g.commit.as_deref(),
            Some(c1.as_str()),
            "the grant pins the commit"
        );
        assert_eq!(reg.status("acme.tool").unwrap().0, Status::Active);

        // Same commit again: the grant survives (identical reviewed content).
        install(&mut reg, Some("v1"));
        assert_eq!(reg.status("acme.tool").unwrap().0, Status::Active);

        // A new upstream commit that changes the manifest: reinstalling it needs a new review.
        std::fs::write(work.join(MANIFEST_FILE), body("2")).unwrap();
        run_git(&work, &["commit", "-qam", "two"]);
        run_git(&work, &["push", "-q", bare.to_str().unwrap(), "main"]);
        let (e, _) = install(&mut reg, None);
        assert_ne!(e.origin.commit.as_deref(), Some(c1.as_str()));
        assert_eq!(reg.status("acme.tool").unwrap().0, Status::Untrusted);
        reg.trust("acme.tool").unwrap();
        assert_eq!(reg.status("acme.tool").unwrap().0, Status::Active);

        // A grant recorded for another commit is stale.
        reg.plugins
            .get_mut("acme.tool")
            .unwrap()
            .trust
            .as_mut()
            .unwrap()
            .commit = Some(c1.clone());
        assert_eq!(reg.status("acme.tool").unwrap().0, Status::StaleTrust);

        // The trust terms show the commit.
        let e = reg.get("acme.tool").unwrap().clone();
        let (m, dg) = read_manifest(&e.root).unwrap();
        assert!(trust_terms(&e, &m, &dg).contains(e.origin.commit.as_deref().unwrap()));
    }
}
