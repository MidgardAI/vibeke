//! The per-user plugin registry with explicit Herdr legacy trust (07 §7.7, 09 §6).
//!
//! * Registrations are per user and shared by every session on the machine, stored atomically
//!   in `plugins.json` next to Vibeke's `config.toml` (default `~/.config/vibeke/plugins.json`).
//!   Installs and links work while no server is running; servers re-read the file.
//! * `install` copies a local plugin directory into a Vibeke-managed checkout; `link` registers a
//!   directory in place. Neither runs any plugin code.
//! * A Herdr plugin is **inactive until trusted**: `trust` records a `herdr_legacy` grant bound to
//!   the manifest's SHA-256 and root. Any manifest change makes the grant stale (re-review
//!   required). Unlink/uninstall drop the registration and its grant.
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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Origin {
    /// `local` (copied from a directory) or `link`.
    pub kind: String,
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_ref: Option<String>,
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

    /// Atomic replace (tmp + rename, 0600).
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
        let _ = std::fs::remove_dir_all(&dest);
        std::fs::rename(&staging, &dest)?;
        let prev = self.plugins.get(&m.id).cloned();
        let entry = Entry {
            id: m.id.clone(),
            kind: "herdr".into(),
            root: dest,
            managed: true,
            origin: Origin {
                kind: "local".into(),
                path: root,
                requested_ref: requested_ref.map(str::to_string),
            },
            enabled: prev.as_ref().is_none_or(|p| p.enabled),
            built: false,
            installed_at_ms: now_ms(),
            // Kept only if the digest still matches (status() checks it): reinstalling the same
            // reviewed source keeps its grant; any change requires review.
            trust: prev.and_then(|p| p.trust),
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
        let g = Grant {
            mode: "herdr_legacy".into(),
            manifest_sha256: digest,
            root: e.root.clone(),
            granted_at_ms: now_ms(),
            entrypoints: m.entrypoints(super::current_platform()),
            baseline: BASELINE_VERSION.into(),
        };
        e.trust = Some(g.clone());
        Ok(g)
    }

    pub fn revoke(&mut self, id: &str) -> Result<Entry, RegistryError> {
        let e = self
            .plugins
            .get_mut(id)
            .ok_or_else(|| RegistryError::NotFound(id.to_string()))?;
        e.trust = None;
        Ok(e.clone())
    }

    pub fn mark_built(&mut self, id: &str) {
        if let Some(e) = self.plugins.get_mut(id) {
            e.built = true;
        }
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
    let status = match &e.trust {
        None => Status::Untrusted,
        Some(g) if g.manifest_sha256 != digest || g.root != e.root || m.id != e.id => {
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
         plugins' identities. Any change to the manifest requires a new review.\n",
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
}
