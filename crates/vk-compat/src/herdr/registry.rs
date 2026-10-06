//! The per-user plugin registry with explicit Herdr legacy trust (07 §7.7, 09 §6).
//!
//! * Registrations are per user and shared by every session on the machine, stored atomically
//!   in `plugins.json` next to Vibeke's `config.toml` (default `~/.config/vibeke/plugins.json`).
//!   Installs and links work while no server is running; servers re-read the file.
//! * `install` copies a local plugin directory into a Vibeke-managed checkout; `link` registers a
//!   directory in place. Neither runs any plugin code.
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

use std::collections::BTreeMap;
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
    /// The manifest file is missing or no longer parses.
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
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let d = Sha256::digest(bytes);
    d.iter().map(|b| format!("{b:02x}")).collect()
}

fn now_ms() -> i64 {
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
fn new_grant_id() -> String {
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

/// Digest of the files below `root` that the manifest's commands reference (an argument that
/// names an existing file inside the plugin root, such as `bin/hook.sh` or `dist/index.js`),
/// with their executable bits and content. Cheap enough to recompute on every status read.
pub fn entry_digest(root: &Path, m: &Manifest) -> String {
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
    let mut h = Sha256::new();
    for f in files {
        let full = root.join(&f);
        let Ok(meta) = std::fs::symlink_metadata(&full) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            h.update(f.to_string_lossy().as_bytes());
            h.update(b"\0l");
            if let Ok(t) = std::fs::read_link(&full) {
                h.update(t.to_string_lossy().as_bytes());
            }
        } else if meta.is_file() {
            h.update(f.to_string_lossy().as_bytes());
            h.update(format!("\0f{:o}", exec_bits(&meta)).as_bytes());
            match std::fs::read(&full) {
                Ok(b) => h.update(Sha256::digest(b)),
                Err(_) => h.update(b"unreadable"),
            }
        } else {
            continue;
        }
        h.update([0]);
    }
    hex(&h.finalize())
}

/// An exclusive advisory lock on `plugins.json.lock`, held while the guard lives.
struct RegistryLock {
    _file: std::fs::File,
}

impl RegistryLock {
    fn acquire(dirs: &PluginDirs) -> io::Result<Self> {
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
                // SAFETY: flock on a descriptor we own; blocks until the lock is free.
                if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
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

fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
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

fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
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

impl Registry {
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
        Ok(())
    }

    pub fn get(&self, id: &str) -> Result<&Entry, RegistryError> {
        self.plugins
            .get(id)
            .ok_or_else(|| RegistryError::NotFound(id.to_string()))
    }

    /// Copy a local plugin directory into a managed checkout and register it (untrusted).
    /// Reinstalling a managed plugin replaces its checkout; a link is never replaced silently.
    pub fn install(
        &mut self,
        dirs: &PluginDirs,
        src: &Path,
        requested_ref: Option<&str>,
    ) -> Result<(Entry, Manifest), RegistryError> {
        let root = plugin_root(src)?;
        let origin = Origin {
            kind: "local".into(),
            path: root.clone(),
            requested_ref: requested_ref.map(str::to_string),
            repo: None,
            commit: None,
        };
        self.install_checkout(dirs, &root, origin)
    }

    /// Register a fetched `owner/repo` checkout ([`super::source::fetch`]) as a managed
    /// install: the source is the repository URL, the pinned value the resolved commit.
    pub fn install_git(
        &mut self,
        dirs: &PluginDirs,
        fetched: &super::source::Fetched,
        src: &super::source::GitSource,
    ) -> Result<(Entry, Manifest), RegistryError> {
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
        self.install_checkout(dirs, &root, origin)
    }

    fn install_checkout(
        &mut self,
        dirs: &PluginDirs,
        root: &Path,
        origin: Origin,
    ) -> Result<(Entry, Manifest), RegistryError> {
        let root = root.to_path_buf();
        let (m, _) = read_manifest(&root)?;
        if let Some(old) = self.plugins.get(&m.id)
            && !old.managed
        {
            return Err(RegistryError::Conflict(format!(
                "{} is linked from {}; unlink it before installing",
                m.id,
                old.root.display()
            )));
        }
        let dest = dirs.checkouts.join(&m.id);
        if dest.starts_with(&root) || root.starts_with(&dest) {
            return Err(RegistryError::Conflict(
                "source and managed checkout overlap".into(),
            ));
        }
        let staging = dirs.checkouts.join(format!(".{}.staging", m.id));
        let _ = std::fs::remove_dir_all(&staging);
        copy_tree(&root, &staging)?;
        // The copy must carry the manifest we validated.
        let (m2, _) = read_manifest(&staging)?;
        if m2 != m {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(RegistryError::Conflict(
                "manifest changed while copying".into(),
            ));
        }
        let tree = match tree_digest(&staging) {
            Ok(t) => t,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&staging);
                return Err(e.into());
            }
        };
        let _ = std::fs::remove_dir_all(&dest);
        std::fs::rename(&staging, &dest)?;
        let prev = self.plugins.get(&m.id).cloned();
        // The grant survives a reinstall only for the exact reviewed content from the same
        // source, and only when there is nothing to rebuild (the fresh checkout is unbuilt).
        let needs_build = !m.build_on(super::current_platform()).is_empty();
        let trust = prev.as_ref().and_then(|p| p.trust.clone()).filter(|g| {
            !needs_build
                && g.source == origin.path
                && g.commit == origin.commit
                && !g.tree_sha256.is_empty()
                && g.tree_sha256 == tree
        });
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
            let _ = std::fs::remove_dir_all(&e.root);
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
            tree_sha256: tree,
            entry_sha256: entry_digest(&e.root, &m),
            commit: e.origin.commit.clone(),
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
        g.entry_sha256 = entry_digest(&e.root, &m);
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
        let t = tempfile::tempdir().unwrap();
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

        // Editing the managed manifest invalidates the grant.
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
        let t = tempfile::tempdir().unwrap();
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
        let t = tempfile::tempdir().unwrap();
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
        let t = tempfile::tempdir().unwrap();
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
        std::fs::write(root.join("bin/go"), "#!/bin/sh\necho changed\n").unwrap();
        assert_eq!(r.status("acme.five").unwrap().0, Status::StaleTrust);
    }

    #[test]
    fn plugins_with_a_build_need_a_build_for_each_grant() {
        let t = tempfile::tempdir().unwrap();
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
        let t = tempfile::tempdir().unwrap();
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

    #[test]
    fn trust_terms_list_entrypoints() {
        let t = tempfile::tempdir().unwrap();
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
        let t = tempfile::tempdir().unwrap();
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
