//! `plugins.lock` (09 §6): a commit-pinned, human-readable record of every registered plugin
//! next to `plugins.json`, rewritten on every registry save. It is informational and diffable
//! (dotfiles repos): what is installed, from where, at which commit and manifest digest. The
//! registry stays authoritative; a lock file never installs anything by itself.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::herdr::registry::{PluginDirs, Registry, write_private};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Locked {
    pub id: String,
    /// `herdr` or `native`.
    pub kind: String,
    /// `local`, `git` or `link`.
    pub origin: String,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// The reviewed manifest digest (legacy grant or native consent), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_sha256: Option<String>,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct LockFile {
    pub version: u32,
    pub generation: u64,
    #[serde(default, rename = "plugin")]
    pub plugins: Vec<Locked>,
}

pub fn path(dirs: &PluginDirs) -> PathBuf {
    dirs.registry.with_file_name("plugins.lock")
}

pub fn build(reg: &Registry) -> LockFile {
    let mut plugins: Vec<Locked> = reg
        .plugins
        .values()
        .map(|e| Locked {
            id: e.id.clone(),
            kind: "herdr".into(),
            origin: e.origin.kind.clone(),
            source: e.origin.path.display().to_string(),
            repo: e.origin.repo.clone(),
            requested_ref: e.origin.requested_ref.clone(),
            commit: e.origin.commit.clone(),
            manifest_sha256: e.trust.as_ref().map(|g| g.manifest_sha256.clone()),
            enabled: e.enabled,
        })
        .collect();
    plugins.extend(reg.native.values().map(|e| Locked {
        id: e.id.clone(),
        kind: "native".into(),
        origin: e.origin.kind.clone(),
        source: e.origin.path.display().to_string(),
        repo: e.origin.repo.clone(),
        requested_ref: e.origin.requested_ref.clone(),
        commit: e.origin.commit.clone(),
        manifest_sha256: e.consent.as_ref().map(|c| c.manifest_sha256.clone()),
        enabled: e.enabled,
    }));
    plugins.sort_by(|a, b| a.id.cmp(&b.id));
    LockFile {
        version: 1,
        generation: reg.generation,
        plugins,
    }
}

/// Rewrite the lock file for `reg` (0600, atomic replace).
pub fn write(dirs: &PluginDirs, reg: &Registry) -> std::io::Result<()> {
    let text = format!(
        "# Written by Vibeke on every plugin registry change. Informational: plugins.json is\n# authoritative; nothing is installed from this file.\n{}",
        toml::to_string_pretty(&build(reg)).map_err(std::io::Error::other)?
    );
    write_private(&path(dirs), text.as_bytes())
}

pub fn read(dirs: &PluginDirs) -> Option<LockFile> {
    toml::from_str(&std::fs::read_to_string(path(dirs)).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_file_follows_every_save() {
        let t = tempfile::tempdir().unwrap();
        let d = PluginDirs {
            registry: t.path().join("plugins.json"),
            checkouts: t.path().join("co"),
            config: t.path().join("cfg"),
            state: t.path().join("st"),
        };
        let src = t.path().join("p");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join(super::super::MANIFEST_FILE),
            "id = \"acme.lock\"\nversion = \"1.2.3\"\n",
        )
        .unwrap();
        Registry::update(&d, |r| r.native_link(&src).map(|_| ())).unwrap();
        let l = read(&d).unwrap();
        assert_eq!(l.generation, 1);
        assert_eq!(l.plugins.len(), 1);
        assert_eq!(l.plugins[0].kind, "native");
        assert_eq!(l.plugins[0].origin, "link");
        assert!(l.plugins[0].manifest_sha256.is_none());
        Registry::update(&d, |r| r.native_consent("acme.lock", None).map(|_| ())).unwrap();
        let l = read(&d).unwrap();
        assert_eq!(l.generation, 2);
        assert!(l.plugins[0].manifest_sha256.is_some());
    }
}
