//! Plugin backup/export (02 §3 "Plugin state ownership": the global registry and plugin-owned
//! files are not reconstructed from a session event log, so a backup includes them explicitly).
//!
//! `export` writes a directory:
//!
//! ```text
//! <out>/export.json          what was exported (ids, generation, time)
//! <out>/plugins.json         the registry (Herdr and native entries, grants and consents)
//! <out>/plugins.lock
//! <out>/config/<id>/…        plugin config dirs
//! <out>/state/<id>/…         plugin state dirs (private sandbox dirs left out)
//! <out>/checkouts/<id>/…     the managed checkout each entry points at
//! ```
//!
//! `import` restores entries whose id is not registered yet and copies config/state files that
//! do not exist yet; everything already present is a reported conflict and left alone. Imported
//! managed checkouts get a new immutable path, so Herdr legacy grants (bound to the root) need
//! a new review; native consents carry over. Links are imported only when their directory
//! exists. Import never touches Herdr's own data.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::herdr::registry::{
    PluginDirs, Registry, RegistryError, copy_tree, nonce, now_ms, set_tree_writable,
};

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Report {
    pub plugins: Vec<String>,
    pub files: usize,
    pub conflicts: Vec<String>,
    /// Imported Herdr plugins whose grant no longer matches (new root): review again.
    pub needs_review: Vec<String>,
    pub skipped: Vec<String>,
}

fn copy_missing(from: &Path, to: &Path, rep: &mut Report, label: &str) -> std::io::Result<()> {
    if !from.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let name = e.file_name();
        if name == ".sandbox" {
            continue;
        }
        let (src, dst) = (e.path(), to.join(&name));
        let ft = e.file_type()?;
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            copy_missing(&src, &dst, rep, label)?;
        } else if std::fs::symlink_metadata(&dst).is_ok() {
            rep.conflicts
                .push(format!("{label}: {} exists", dst.display()));
        } else {
            std::fs::copy(&src, &dst)?;
            rep.files += 1;
        }
    }
    Ok(())
}

/// Export the registry and every plugin's config/state and managed checkout into `out` (which
/// must not exist or be empty).
pub fn export(dirs: &PluginDirs, out: &Path) -> Result<Report, RegistryError> {
    if out.exists() && std::fs::read_dir(out)?.next().is_some() {
        return Err(RegistryError::Conflict(format!(
            "{} is not empty",
            out.display()
        )));
    }
    std::fs::create_dir_all(out)?;
    let reg = Registry::load_shared(dirs)?;
    let mut rep = Report::default();
    let text =
        serde_json::to_vec_pretty(&reg).map_err(|e| RegistryError::Corrupt(e.to_string()))?;
    std::fs::write(out.join("plugins.json"), text)?;
    let _ = std::fs::copy(super::lockfile::path(dirs), out.join("plugins.lock"));
    let mut ids: Vec<(String, PathBuf, bool)> = reg
        .plugins
        .values()
        .map(|e| (e.id.clone(), e.root.clone(), e.managed))
        .collect();
    ids.extend(
        reg.native
            .values()
            .map(|e| (e.id.clone(), e.root.clone(), e.managed)),
    );
    for (id, root, managed) in &ids {
        copy_missing(
            &dirs.config_dir(id),
            &out.join("config").join(id),
            &mut rep,
            id,
        )?;
        copy_missing(
            &dirs.state_dir(id),
            &out.join("state").join(id),
            &mut rep,
            id,
        )?;
        if *managed && root.is_dir() {
            let name = root
                .file_name()
                .map(|n| n.to_os_string())
                .unwrap_or_default();
            let dest = out.join("checkouts").join(id).join(name);
            copy_tree(root, &dest)?;
            let _ = set_tree_writable(&dest, true);
        }
        rep.plugins.push(id.clone());
    }
    let meta = serde_json::json!({
        "version": 1,
        "exported_at_ms": now_ms(),
        "generation": reg.generation,
        "plugins": rep.plugins,
    });
    std::fs::write(
        out.join("export.json"),
        serde_json::to_vec_pretty(&meta).unwrap_or_default(),
    )?;
    Ok(rep)
}

/// Import an export directory (see the module docs). `dry_run` reports without writing.
pub fn import(dirs: &PluginDirs, from: &Path, dry_run: bool) -> Result<Report, RegistryError> {
    let text = std::fs::read_to_string(from.join("plugins.json"))?;
    let src: Registry =
        serde_json::from_str(&text).map_err(|e| RegistryError::Corrupt(e.to_string()))?;
    let mut rep = Report::default();
    let current = Registry::load_shared(dirs)?;
    let checkout_of = |id: &str, root: &Path| -> Option<PathBuf> {
        let name = root.file_name()?;
        let p = from.join("checkouts").join(id).join(name);
        p.is_dir().then_some(p)
    };
    // Plan first (no writes on a dry run).
    let mut herdr = vec![];
    let mut native = vec![];
    for e in src.plugins.values() {
        if current.plugins.contains_key(&e.id) || current.native.contains_key(&e.id) {
            rep.conflicts.push(format!("{}: already registered", e.id));
        } else if e.managed && checkout_of(&e.id, &e.root).is_none() {
            rep.skipped
                .push(format!("{}: checkout missing from the export", e.id));
        } else if !e.managed && !e.root.is_dir() {
            rep.skipped.push(format!(
                "{}: linked directory {} missing",
                e.id,
                e.root.display()
            ));
        } else {
            herdr.push(e.clone());
        }
    }
    for e in src.native.values() {
        if current.plugins.contains_key(&e.id) || current.native.contains_key(&e.id) {
            rep.conflicts.push(format!("{}: already registered", e.id));
        } else if e.managed && checkout_of(&e.id, &e.root).is_none() {
            rep.skipped
                .push(format!("{}: checkout missing from the export", e.id));
        } else if !e.managed && !e.root.is_dir() {
            rep.skipped.push(format!(
                "{}: linked directory {} missing",
                e.id,
                e.root.display()
            ));
        } else {
            native.push(e.clone());
        }
    }
    if dry_run {
        rep.plugins = herdr
            .iter()
            .map(|e| e.id.clone())
            .chain(native.iter().map(|e| e.id.clone()))
            .collect();
        return Ok(rep);
    }
    let place = |id: &str, root: &Path| -> Result<PathBuf, RegistryError> {
        let src = checkout_of(id, root).ok_or_else(|| RegistryError::NotFound(id.into()))?;
        let dest = dirs.checkouts.join(id).join(format!("import-{}", nonce()));
        copy_tree(&src, &dest)?;
        set_tree_writable(&dest, false)?;
        Ok(dest)
    };
    let mut placed_h = vec![];
    for mut e in herdr {
        if e.managed {
            e.root = place(&e.id, &e.root)?;
            if e.trust.is_some() {
                rep.needs_review.push(e.id.clone());
            }
        }
        placed_h.push(e);
    }
    let mut placed_n = vec![];
    for mut e in native {
        if e.managed {
            e.root = place(&e.id, &e.root)?;
        }
        placed_n.push(e);
    }
    let ids = Registry::update(dirs, |r| {
        let mut ids = vec![];
        for e in placed_h {
            if !r.plugins.contains_key(&e.id) && !r.native.contains_key(&e.id) {
                ids.push(e.id.clone());
                r.plugins.insert(e.id.clone(), e);
            }
        }
        for e in placed_n {
            if !r.plugins.contains_key(&e.id) && !r.native.contains_key(&e.id) {
                ids.push(e.id.clone());
                r.native.insert(e.id.clone(), e);
            }
        }
        Ok(ids)
    })?;
    for id in &ids {
        copy_missing(
            &from.join("config").join(id),
            &dirs.config_dir(id),
            &mut rep,
            id,
        )?;
        copy_missing(
            &from.join("state").join(id),
            &dirs.state_dir(id),
            &mut rep,
            id,
        )?;
    }
    rep.plugins = ids;
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::registry::{local_origin, stage};

    fn dirs(t: &Path) -> PluginDirs {
        PluginDirs {
            registry: t.join("cfg/plugins.json"),
            checkouts: t.join("st/checkouts"),
            config: t.join("cfg/plugins"),
            state: t.join("st/state"),
        }
    }

    #[test]
    fn export_then_import_elsewhere() {
        let t = tempfile::tempdir().unwrap();
        let a = dirs(&t.path().join("a"));
        let src = t.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join(super::super::MANIFEST_FILE),
            "id = \"acme.bk\"\nversion = \"1.0.0\"\n[capabilities]\nstorage = true\n",
        )
        .unwrap();
        let staged = stage(&a, &src, local_origin(&src, None)).unwrap();
        Registry::update(&a, |r| r.native_install_staged(&a, staged).map(|_| ())).unwrap();
        Registry::update(&a, |r| r.native_consent("acme.bk", None).map(|_| ())).unwrap();
        std::fs::create_dir_all(a.state_dir("acme.bk").join(".sandbox")).unwrap();
        std::fs::write(a.state_dir("acme.bk").join("db.json"), "{}").unwrap();
        std::fs::write(a.state_dir("acme.bk").join(".sandbox/tmp"), "x").unwrap();
        let out = t.path().join("export");
        let rep = export(&a, &out).unwrap();
        assert_eq!(rep.plugins, vec!["acme.bk"]);
        assert!(out.join("state/acme.bk/db.json").is_file());
        assert!(
            !out.join("state/acme.bk/.sandbox").exists(),
            "private dirs left out"
        );
        assert!(export(&a, &out).is_err(), "never into a non-empty dir");

        let b = dirs(&t.path().join("b"));
        let dry = import(&b, &out, true).unwrap();
        assert_eq!(dry.plugins, vec!["acme.bk"]);
        assert!(
            Registry::load(&b).unwrap().native.is_empty(),
            "dry run writes nothing"
        );
        let rep = import(&b, &out, false).unwrap();
        assert_eq!(rep.plugins, vec!["acme.bk"]);
        let reg = Registry::load(&b).unwrap();
        let e = reg.native_get("acme.bk").unwrap();
        assert!(e.root.starts_with(&b.checkouts));
        assert!(e.consent.is_some(), "native consent carries over");
        assert!(b.state_dir("acme.bk").join("db.json").is_file());
        // A second import conflicts and changes nothing.
        let again = import(&b, &out, false).unwrap();
        assert!(again.plugins.is_empty());
        assert_eq!(again.conflicts.len(), 1);
    }
}
