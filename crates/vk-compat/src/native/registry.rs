//! Native plugin registrations (07 §7.6, 09 §6), stored in the shared per-user `plugins.json`
//! (`Registry::native`), under the same lock and generation as the Herdr entries.
//!
//! * `install` copies a plugin directory (or a fetched repository) into a fresh immutable
//!   managed checkout `<checkouts>/<id>/<commit|local>-<nonce>/` exactly like the Herdr path;
//!   `link` registers a development directory in place (hot restart, 07 §7.6). Neither runs code.
//! * A plugin is **inactive until consented**: [`Registry::native_consent`] records the approved
//!   capabilities with the manifest digest and the source commit. A later manifest that asks for
//!   anything the consent does not cover ([`super::caps::Capabilities::widened_from`]) is
//!   `reconsent` until the user approves again; a narrower one stays active (narrowing is
//!   silent, 09 §6). The capabilities in force are always the manifest's (never more than the
//!   consent).
//! * `[[build]]` runs once after consent (without any token, socket or pane identity); the
//!   managed tree digest is recorded after it and re-verified before every launch.
//! * Ids are unique across native and Herdr registrations.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::caps::Capabilities;
use super::manifest::{Manifest, ManifestError};
use super::MANIFEST_FILE;
use crate::herdr::registry::{
    Origin, PluginDirs, Registry, RegistryError, copy_tree, escaping_symlink, new_grant_id,
    nonce, now_ms, remove_checkout, set_tree_writable, sha256_hex, tree_digest,
    tree_digest_cached,
};

impl From<ManifestError> for RegistryError {
    fn from(e: ManifestError) -> Self {
        RegistryError::Conflict(e.to_string())
    }
}

/// The user's approval of a capability set (09 §6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Consent {
    /// Unique per consent: a revoke + new consent never matches tokens of the old one.
    pub consent_id: String,
    pub capabilities: Capabilities,
    /// The manifest the user reviewed.
    pub manifest_sha256: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    pub granted_at_ms: i64,
    /// Entrypoints shown at consent.
    #[serde(default)]
    pub entrypoints: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NativeEntry {
    pub id: String,
    pub root: PathBuf,
    /// Vibeke-owned checkout (install) rather than the user's directory (link).
    pub managed: bool,
    pub origin: Origin,
    pub enabled: bool,
    /// `[[build]]` ran successfully for this checkout under the current consent.
    #[serde(default)]
    pub built: bool,
    pub installed_at_ms: i64,
    /// Whole-tree digest a launch of a managed checkout must find (after the build).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tree_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consent: Option<Consent>,
}

/// Effective state of a native registration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "status", content = "detail")]
pub enum NativeStatus {
    /// Consented, built, enabled: actions, hooks and the process run.
    Active,
    Disabled,
    /// No consent yet.
    NeedsConsent,
    /// The manifest asks for capabilities the consent does not cover.
    Reconsent(Vec<String>),
    /// Consented but `[[build]]` has not run for this checkout.
    Unbuilt,
    /// Wrong platform or too old a Vibeke.
    Incompatible(String),
    /// Manifest missing or invalid.
    Broken(String),
}

impl NativeStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            NativeStatus::Active => "active",
            NativeStatus::Disabled => "disabled",
            NativeStatus::NeedsConsent => "needs_consent",
            NativeStatus::Reconsent(_) => "reconsent",
            NativeStatus::Unbuilt => "unbuilt",
            NativeStatus::Incompatible(_) => "incompatible",
            NativeStatus::Broken(_) => "broken",
        }
    }
    /// The reason text for statuses that carry one.
    pub fn detail(&self) -> Option<String> {
        match self {
            NativeStatus::Reconsent(w) => Some(format!("widened: {}", w.join(", "))),
            NativeStatus::Incompatible(s) | NativeStatus::Broken(s) => Some(s.clone()),
            _ => None,
        }
    }
}

/// The manifest and its digest.
pub fn read_manifest(root: &Path) -> Result<(Manifest, String), RegistryError> {
    let f = root.join(MANIFEST_FILE);
    let text = std::fs::read_to_string(&f).map_err(|e| {
        RegistryError::Io(std::io::Error::new(e.kind(), format!("{}: {e}", f.display())))
    })?;
    let m = Manifest::parse(&text)?;
    Ok((m, sha256_hex(text.as_bytes())))
}

fn plugin_root(src: &Path) -> Result<PathBuf, RegistryError> {
    let p = if src.is_file() {
        src.parent().unwrap_or(Path::new(".")).to_path_buf()
    } else {
        src.to_path_buf()
    };
    let p = std::fs::canonicalize(&p).map_err(|e| {
        RegistryError::Io(std::io::Error::new(e.kind(), format!("{}: {e}", p.display())))
    })?;
    if !p.join(MANIFEST_FILE).is_file() {
        return Err(RegistryError::Conflict(format!(
            "{} has no {MANIFEST_FILE}",
            p.display()
        )));
    }
    Ok(p)
}

/// A native plugin copied into the staging area and verified, not registered yet.
#[derive(Debug)]
pub struct NativeStaged {
    pub dir: PathBuf,
    pub manifest: Manifest,
    pub manifest_sha256: String,
    pub tree_sha256: String,
    pub origin: Origin,
}

impl NativeStaged {
    pub fn discard(self) {
        let _ = remove_checkout(&self.dir);
    }
}

/// Copy `src` into `<checkouts>/.staging/`: no symlink may point outside it and the copied
/// manifest must equal the source's.
pub fn stage(dirs: &PluginDirs, src: &Path, origin: Origin) -> Result<NativeStaged, RegistryError> {
    let root = plugin_root(src)?;
    let (m, digest) = read_manifest(&root)?;
    if root.starts_with(&dirs.checkouts) {
        return Err(RegistryError::Conflict(
            "source and managed checkout overlap".into(),
        ));
    }
    if let Some(link) = escaping_symlink(&root)? {
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
    if let Err(e) = copy_tree(&root, &dir) {
        return fail(e.into(), &dir);
    }
    match read_manifest(&dir) {
        Ok((_, d2)) if d2 == digest => {}
        Ok(_) => {
            return fail(
                RegistryError::Conflict("manifest changed while copying".into()),
                &dir,
            );
        }
        Err(e) => return fail(e, &dir),
    }
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
    let origin = Origin {
        path: if origin.kind == "local" {
            root
        } else {
            origin.path
        },
        ..origin
    };
    Ok(NativeStaged {
        dir,
        manifest: m,
        manifest_sha256: digest,
        tree_sha256: tree,
        origin,
    })
}

/// A local-directory origin.
pub fn local_origin(src: &Path, requested_ref: Option<&str>) -> Origin {
    Origin {
        kind: "local".into(),
        path: src.to_path_buf(),
        requested_ref: requested_ref.map(str::to_string),
        repo: None,
        commit: None,
    }
}

/// The origin of a fetched `owner/repo` checkout (source = repository URL, pinned commit).
pub fn git_origin(
    fetched: &crate::herdr::source::Fetched,
    src: &crate::herdr::source::GitSource,
) -> Origin {
    let mut path = PathBuf::from(&fetched.url);
    if let Some(s) = &src.subdir {
        path = path.join(s);
    }
    Origin {
        kind: "git".into(),
        path,
        requested_ref: src.git_ref.clone(),
        repo: Some(src.spec()),
        commit: Some(fetched.commit.clone()),
    }
}

/// Status of one native entry for this Vibeke version and platform.
pub fn native_status(e: &NativeEntry, vibeke_version: &str) -> (NativeStatus, Option<Manifest>) {
    let (m, _digest) = match read_manifest(&e.root) {
        Ok(x) => x,
        Err(err) => return (NativeStatus::Broken(err.to_string()), None),
    };
    if m.id != e.id {
        return (
            NativeStatus::Broken(format!("manifest id changed to {}", m.id)),
            Some(m),
        );
    }
    if let Err(why) = m.compatible(vibeke_version, crate::herdr::current_platform()) {
        return (NativeStatus::Incompatible(why), Some(m));
    }
    let st = match &e.consent {
        None => NativeStatus::NeedsConsent,
        Some(c) => {
            let widened = m.capabilities.widened_from(&c.capabilities);
            if !widened.is_empty() {
                NativeStatus::Reconsent(widened)
            } else if e.managed
                && !e.built
                && !m.build_on(crate::herdr::current_platform()).is_empty()
            {
                NativeStatus::Unbuilt
            } else if !e.enabled {
                NativeStatus::Disabled
            } else {
                NativeStatus::Active
            }
        }
    };
    (st, Some(m))
}

/// Before a launch of a managed checkout: the whole tree must still be the recorded one.
pub fn verify_launch(e: &NativeEntry) -> Result<(), String> {
    if !e.managed {
        return Ok(());
    }
    if e.tree_sha256.is_empty() {
        return Err(format!("{} has no recorded tree digest; reinstall it", e.id));
    }
    let actual =
        tree_digest_cached(&e.root).map_err(|err| format!("{}: {err}", e.root.display()))?;
    if actual != e.tree_sha256 {
        return Err(format!(
            "{}'s checkout {} changed since it was installed (whole-tree digest); not started. Reinstall it",
            e.id,
            e.root.display()
        ));
    }
    Ok(())
}

/// What changed between the consent and the manifest on disk (shown by `plugin update`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CapabilityDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    /// `added` widens the consent: re-consent required.
    pub widens: bool,
}

pub fn diff(old: &Capabilities, new: &Capabilities) -> CapabilityDiff {
    let added = new.widened_from(old);
    let removed = old.widened_from(new);
    CapabilityDiff {
        widens: !added.is_empty(),
        added,
        removed,
    }
}

impl Registry {
    pub fn native_get(&self, id: &str) -> Result<&NativeEntry, RegistryError> {
        self.native
            .get(id)
            .ok_or_else(|| RegistryError::NotFound(id.to_string()))
    }

    fn native_mut(&mut self, id: &str) -> Result<&mut NativeEntry, RegistryError> {
        self.native
            .get_mut(id)
            .ok_or_else(|| RegistryError::NotFound(id.to_string()))
    }

    /// Publish a staged checkout at its immutable path and point the entry at it. The consent
    /// is kept (its capabilities decide whether the new manifest is active or `reconsent`); a
    /// new checkout always needs its build again.
    pub fn native_install_staged(
        &mut self,
        dirs: &PluginDirs,
        staged: NativeStaged,
    ) -> Result<(NativeEntry, Manifest), RegistryError> {
        let m = staged.manifest.clone();
        if self.plugins.contains_key(&m.id) {
            let msg = format!("{} is registered as a Herdr plugin; remove it first", m.id);
            staged.discard();
            return Err(RegistryError::Conflict(msg));
        }
        if let Some(old) = self.native.get(&m.id)
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
        let moved = std::fs::create_dir_all(&id_dir)
            .and_then(|_| std::fs::rename(&staged.dir, &dest))
            .and_then(|_| set_tree_writable(&dest, false));
        if let Err(e) = moved {
            let _ = remove_checkout(&dest);
            staged.discard();
            return Err(e.into());
        }
        let prev = self.native.get(&m.id).cloned();
        let needs_build = !m.build_on(crate::herdr::current_platform()).is_empty();
        let entry = NativeEntry {
            id: m.id.clone(),
            root: dest,
            managed: true,
            origin: staged.origin,
            enabled: prev.as_ref().is_none_or(|p| p.enabled),
            built: !needs_build,
            installed_at_ms: now_ms(),
            tree_sha256: staged.tree_sha256,
            consent: prev.and_then(|p| p.consent),
        };
        self.native.insert(m.id.clone(), entry.clone());
        Ok((entry, m))
    }

    /// Register a development directory in place (never builds).
    pub fn native_link(&mut self, path: &Path) -> Result<(NativeEntry, Manifest), RegistryError> {
        let root = plugin_root(path)?;
        let (m, _) = read_manifest(&root)?;
        if self.plugins.contains_key(&m.id) {
            return Err(RegistryError::Conflict(format!(
                "{} is registered as a Herdr plugin; remove it first",
                m.id
            )));
        }
        if let Some(old) = self.native.get(&m.id) {
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
        let prev = self.native.get(&m.id).cloned();
        let entry = NativeEntry {
            id: m.id.clone(),
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
            built: true,
            installed_at_ms: prev.as_ref().map_or_else(now_ms, |p| p.installed_at_ms),
            tree_sha256: String::new(),
            consent: prev.and_then(|p| p.consent),
        };
        self.native.insert(m.id.clone(), entry.clone());
        Ok((entry, m))
    }

    /// Remove a registration: a link keeps its files, a managed checkout is deleted (plugin
    /// config/state and KV data stay).
    pub fn native_remove(
        &mut self,
        dirs: &PluginDirs,
        id: &str,
    ) -> Result<NativeEntry, RegistryError> {
        let e = self.native_get(id)?.clone();
        if e.managed && e.root.starts_with(&dirs.checkouts) {
            let id_dir = dirs.checkouts.join(id);
            let _ = remove_checkout(&id_dir);
        }
        self.native.remove(id);
        Ok(e)
    }

    pub fn native_set_enabled(&mut self, id: &str, on: bool) -> Result<NativeEntry, RegistryError> {
        let e = self.native_mut(id)?;
        e.enabled = on;
        Ok(e.clone())
    }

    /// Record consent for the manifest on disk. `accepted` (item names, `*` = all shown) must
    /// cover every requested item; `None` means the caller showed the terms and the user
    /// confirmed them all.
    pub fn native_consent(
        &mut self,
        id: &str,
        accepted: Option<&[String]>,
    ) -> Result<Consent, RegistryError> {
        let e = self.native_mut(id)?;
        let (m, digest) = read_manifest(&e.root)?;
        if m.id != e.id {
            return Err(RegistryError::Conflict(format!(
                "manifest id changed to {}; reinstall",
                m.id
            )));
        }
        if let Some(acc) = accepted {
            let missing = m.capabilities.not_accepted(acc);
            if !missing.is_empty() {
                return Err(RegistryError::Conflict(format!(
                    "capabilities_not_accepted: {}",
                    missing.join(", ")
                )));
            }
        }
        let c = Consent {
            consent_id: new_grant_id(),
            capabilities: m.capabilities.clone(),
            manifest_sha256: digest,
            version: m.version.clone(),
            commit: e.origin.commit.clone(),
            granted_at_ms: now_ms(),
            entrypoints: m.entrypoints(crate::herdr::current_platform()),
        };
        let needs_build = !m.build_on(crate::herdr::current_platform()).is_empty();
        // A fresh consent of an unbuilt managed checkout still needs its build.
        if e.managed && needs_build && e.consent.is_none() {
            e.built = false;
        }
        e.consent = Some(c.clone());
        Ok(c)
    }

    pub fn native_revoke(&mut self, id: &str) -> Result<NativeEntry, RegistryError> {
        let e = self.native_mut(id)?;
        e.consent = None;
        Ok(e.clone())
    }

    /// Record a successful build for consent `consent_id` (the build may have produced the
    /// files the process runs, so the tree digest is re-recorded).
    pub fn native_finish_build(&mut self, id: &str, consent_id: &str) -> Result<(), RegistryError> {
        let e = self.native_mut(id)?;
        if e.consent.as_ref().map(|c| c.consent_id.as_str()) != Some(consent_id) {
            return Err(RegistryError::Conflict(format!(
                "the consent for {id} changed during the build"
            )));
        }
        if e.managed {
            e.tree_sha256 = tree_digest(&e.root)?;
        }
        e.built = true;
        Ok(())
    }

    /// Active native plugins with their manifests.
    pub fn native_active(&self, vibeke_version: &str) -> Vec<(NativeEntry, Manifest)> {
        self.native
            .values()
            .filter_map(|e| match native_status(e, vibeke_version) {
                (NativeStatus::Active, Some(m)) => Some((e.clone(), m)),
                _ => None,
            })
            .collect()
    }
}

/// Run `[[build]]` of `e` (managed checkouts only; links never build): argv in the plugin dir,
/// no token, socket, pane identity or plugin context in the environment (07 §7.7 build/runtime
/// separation applies to native plugins too). The manifest must stay `digest` throughout.
pub fn run_build(e: &NativeEntry, m: &Manifest, digest: &str) -> Result<(), String> {
    let steps = m.build_on(crate::herdr::current_platform());
    if !e.managed || steps.is_empty() {
        return Ok(());
    }
    let unchanged = || match read_manifest(&e.root) {
        Ok((_, d)) if d == digest => Ok(()),
        Ok(_) => Err(format!("{}'s manifest changed during the build", e.id)),
        Err(err) => Err(format!("manifest unreadable during the build: {err}")),
    };
    set_tree_writable(&e.root, true).map_err(|err| err.to_string())?;
    let env: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| !(k.starts_with("VIBEKE_") || k.starts_with("HERDR_")))
        .collect();
    let mut result = Ok(());
    for step in steps {
        if let Err(why) = unchanged() {
            result = Err(why);
            break;
        }
        let mut argv = step.command.clone();
        if argv[0].contains('/') && !argv[0].starts_with('/') {
            argv[0] = e.root.join(&argv[0]).to_string_lossy().into_owned();
        }
        let out = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(&e.root)
            .env_clear()
            .envs(env.iter().cloned())
            .stdin(std::process::Stdio::null())
            .output();
        match out {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                result = Err(format!(
                    "build `{}` failed ({}): {}",
                    argv.join(" "),
                    o.status,
                    String::from_utf8_lossy(&o.stderr).trim()
                ));
                break;
            }
            Err(err) => {
                result = Err(format!("build {}: {err}", argv.join(" ")));
                break;
            }
        }
    }
    let _ = set_tree_writable(&e.root, false);
    result.and_then(|()| unchanged())
}

/// Record consent for `id` (see [`Registry::native_consent`]) and run its build if the managed
/// checkout needs one. A failed build revokes the consent again. Each registry step is its own
/// locked read-modify-write; the build runs without the lock and is recorded only for the
/// consent it ran for.
pub fn consent_and_build(
    dirs: &PluginDirs,
    id: &str,
    accepted: Option<&[String]>,
) -> Result<Consent, RegistryError> {
    let c = Registry::update(dirs, |r| r.native_consent(id, accepted))?;
    let reg = Registry::load_shared(dirs)?;
    let e = reg.native_get(id)?.clone();
    let (m, digest) = read_manifest(&e.root)?;
    if e.managed && !e.built && !m.build_on(crate::herdr::current_platform()).is_empty() {
        if let Err(why) = run_build(&e, &m, &digest) {
            let _ = Registry::update(dirs, |r| {
                if r.native.get(id).and_then(|x| x.consent.as_ref()).map(|x| x.consent_id.as_str())
                    == Some(c.consent_id.as_str())
                {
                    r.native_revoke(id)?;
                }
                Ok(())
            });
            return Err(RegistryError::Conflict(format!(
                "{why}; consent revoked"
            )));
        }
        Registry::update(dirs, |r| r.native_finish_build(id, &c.consent_id))?;
    }
    Ok(c)
}

/// The consent terms shown before a native consent (07 §7.6, 09 §6): source and commit,
/// entrypoints, and every requested capability with its risk (high risk marked).
pub fn consent_terms(e: &NativeEntry, m: &Manifest, previous: Option<&Capabilities>) -> String {
    let mut s = format!(
        "Native plugin {} {} ({})\n  source:   {} ({}{})\n",
        m.id,
        m.version,
        m.name.as_deref().unwrap_or(&m.id),
        e.origin.path.display(),
        e.origin.kind,
        e.origin
            .requested_ref
            .as_deref()
            .map(|r| format!(", ref {r}"))
            .unwrap_or_default()
    );
    if let Some(c) = &e.origin.commit {
        s.push_str(&format!("  commit:   {c}\n"));
    }
    s.push_str(&format!("  root:     {}\n", e.root.display()));
    if m.sandbox {
        s.push_str("  sandbox:  yes (OS sandbox generated from the capabilities)\n");
    }
    let eps = m.entrypoints(crate::herdr::current_platform());
    if !eps.is_empty() {
        s.push_str("  entrypoints:\n");
        for ep in eps {
            s.push_str(&format!("    - {ep}\n"));
        }
    }
    let items = m.capabilities.items();
    if items.is_empty() {
        s.push_str("  capabilities: none\n");
    } else {
        s.push_str("  capabilities:\n");
        let new: Vec<String> = previous
            .map(|p| m.capabilities.widened_from(p))
            .unwrap_or_default();
        for i in items {
            let mark = match i.risk {
                super::caps::Risk::High => "HIGH ",
                super::caps::Risk::Medium => "med  ",
                super::caps::Risk::Low => "low  ",
            };
            let added = if new.contains(&i.name) { " (new)" } else { "" };
            s.push_str(&format!(
                "    {mark}{} — {}{added}\n",
                i.name, i.description
            ));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs(t: &Path) -> PluginDirs {
        PluginDirs {
            registry: t.join("cfg/plugins.json"),
            checkouts: t.join("state/checkouts"),
            config: t.join("cfg/plugins"),
            state: t.join("state/state"),
        }
    }

    fn plugin(dir: &Path, caps: &str, extra: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join(MANIFEST_FILE),
            format!(
                "id = \"acme.tool\"\nversion = \"1.0.0\"\n{extra}\n[[actions]]\nid = \"go\"\ntitle = \"Go\"\ncommand = [\"sh\", \"go.sh\"]\n[capabilities]\n{caps}\n"
            ),
        )
        .unwrap();
        std::fs::write(dir.join("go.sh"), "echo hi\n").unwrap();
    }

    #[test]
    fn install_consent_widen_and_narrow() {
        let t = tempfile::tempdir().unwrap();
        let d = dirs(t.path());
        let src = t.path().join("src");
        plugin(&src, "storage = true\nevents_read = [\"agent.*\"]", "");
        let staged = stage(&d, &src, local_origin(&src, None)).unwrap();
        let (e, _) = Registry::update(&d, |r| r.native_install_staged(&d, staged)).unwrap();
        assert!(e.managed && e.root.starts_with(&d.checkouts));
        let reg = Registry::load(&d).unwrap();
        assert_eq!(native_status(&e, "1.0.0").0, NativeStatus::NeedsConsent);
        assert!(reg.native_active("1.0.0").is_empty());
        assert!(reg.plugins.is_empty(), "never visible to the Herdr paths");
        assert_eq!(reg.generation, 1);
        // Consent must cover every item.
        let err = Registry::update(&d, |r| r.native_consent("acme.tool", Some(&["storage".into()])))
            .unwrap_err()
            .to_string();
        assert!(err.contains("capabilities_not_accepted"), "{err}");
        Registry::update(&d, |r| r.native_consent("acme.tool", Some(&["*".into()]))).unwrap();
        let reg = Registry::load(&d).unwrap();
        assert_eq!(reg.native_active("1.0.0").len(), 1);
        verify_launch(reg.native_get("acme.tool").unwrap()).unwrap();
        // An update that narrows stays active; one that widens needs consent again.
        plugin(&src, "events_read = [\"agent.started\"]", "");
        let staged = stage(&d, &src, local_origin(&src, None)).unwrap();
        Registry::update(&d, |r| r.native_install_staged(&d, staged)).unwrap();
        let reg = Registry::load(&d).unwrap();
        assert_eq!(reg.native_active("1.0.0").len(), 1, "narrowing is silent");
        plugin(&src, "panes_write = true", "");
        let staged = stage(&d, &src, local_origin(&src, None)).unwrap();
        Registry::update(&d, |r| r.native_install_staged(&d, staged)).unwrap();
        let reg = Registry::load(&d).unwrap();
        let e = reg.native_get("acme.tool").unwrap();
        assert_eq!(
            native_status(e, "1.0.0").0,
            NativeStatus::Reconsent(vec!["panes_write".into()])
        );
        let terms = consent_terms(e, &read_manifest(&e.root).unwrap().0, Some(&e.consent.as_ref().unwrap().capabilities));
        assert!(terms.contains("HIGH panes_write") && terms.contains("(new)"), "{terms}");
        // Tampering with the managed checkout stops launches.
        set_tree_writable(&e.root, true).unwrap();
        std::fs::write(e.root.join("go.sh"), "echo changed\n").unwrap();
        assert!(verify_launch(e).unwrap_err().contains("changed"));
        // Herdr and native ids never collide.
        let h = t.path().join("h");
        std::fs::create_dir_all(&h).unwrap();
        std::fs::write(
            h.join("herdr-plugin.toml"),
            "id = \"acme.tool\"\nname = \"x\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();
        let err = Registry::update(&d, |r| r.link(&h).map(|_| ())).unwrap_err().to_string();
        assert!(err.contains("native"), "{err}");
        Registry::update(&d, |r| r.native_remove(&d, "acme.tool").map(|_| ())).unwrap();
        assert!(!d.checkouts.join("acme.tool").exists());
    }

    #[test]
    fn link_build_and_compat() {
        let t = tempfile::tempdir().unwrap();
        let d = dirs(t.path());
        let src = t.path().join("dev");
        plugin(&src, "", "min_vibeke = \"9.0.0\"\n[[build]]\ncommand = [\"true\"]");
        let (e, _) = Registry::update(&d, |r| r.native_link(&src)).unwrap();
        assert!(!e.managed && e.built, "links never build");
        Registry::update(&d, |r| r.native_consent("acme.tool", None)).unwrap();
        let reg = Registry::load(&d).unwrap();
        let e = reg.native_get("acme.tool").unwrap();
        assert!(matches!(native_status(e, "1.0.0").0, NativeStatus::Incompatible(_)));
        assert_eq!(native_status(e, "9.0.0").0, NativeStatus::Active);
        // A managed install with a build is `unbuilt` until the build is recorded.
        Registry::update(&d, |r| r.native_remove(&d, "acme.tool").map(|_| ())).unwrap();
        let staged = stage(&d, &src, local_origin(&src, None)).unwrap();
        Registry::update(&d, |r| r.native_install_staged(&d, staged)).unwrap();
        let c = Registry::update(&d, |r| r.native_consent("acme.tool", None)).unwrap();
        let reg = Registry::load(&d).unwrap();
        assert_eq!(
            native_status(reg.native_get("acme.tool").unwrap(), "9.0.0").0,
            NativeStatus::Unbuilt
        );
        assert!(Registry::update(&d, |r| r.native_finish_build("acme.tool", "other")).is_err());
        Registry::update(&d, |r| r.native_finish_build("acme.tool", &c.consent_id)).unwrap();
        let reg = Registry::load(&d).unwrap();
        assert_eq!(
            native_status(reg.native_get("acme.tool").unwrap(), "9.0.0").0,
            NativeStatus::Active
        );
        let df = diff(
            &Capabilities::default(),
            &Capabilities {
                storage: true,
                ..Default::default()
            },
        );
        assert!(df.widens && df.added == vec!["storage"]);
    }
}
