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
//! managed checkouts get a new immutable path. Neither Herdr legacy grants nor native consents
//! carry over (the foreign registry is not trusted: grants, consent and build state are dropped,
//! native tree digests are recomputed from the imported files, and every imported plugin needs a
//! new review). Plugin ids from the export are validated before any path is built. Links are imported only when their directory
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

/// A plugin id that is safe to use as one path component under a Vibeke directory. Native ids
/// must also satisfy the manifest rule.
fn safe_id(id: &str, native: bool) -> bool {
    let ok = !id.is_empty()
        && id.len() <= 128
        && !id.contains("..")
        && id.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    ok && (!native || crate::native::manifest::valid_id(id))
}

/// `root/<rel>` where every component of `rel` is a plain name (no `..`, no root, no prefix).
fn join_under(root: &Path, rel: &Path) -> Result<PathBuf, RegistryError> {
    if rel
        .components()
        .all(|c| matches!(c, std::path::Component::Normal(_)))
    {
        Ok(root.join(rel))
    } else {
        Err(RegistryError::Corrupt(format!(
            "path {} escapes {}",
            rel.display(),
            root.display()
        )))
    }
}

/// Import an export directory (see the module docs). `dry_run` reports without writing.
pub fn import(dirs: &PluginDirs, from: &Path, dry_run: bool) -> Result<Report, RegistryError> {
    let text = std::fs::read_to_string(from.join("plugins.json"))?;
    let src: Registry =
        serde_json::from_str(&text).map_err(|e| RegistryError::Corrupt(e.to_string()))?;
    let mut rep = Report::default();
    let current = Registry::load_shared(dirs)?;
    let checkouts_root = std::fs::canonicalize(from.join("checkouts")).ok();
    let checkout_of = |id: &str, root: &Path| -> Option<PathBuf> {
        let name = root.file_name()?;
        let p = join_under(&from.join("checkouts"), &Path::new(id).join(name)).ok()?;
        // Still under the export's checkouts directory once symlinks are resolved.
        let real = std::fs::canonicalize(&p).ok()?;
        (real.is_dir() && real.starts_with(checkouts_root.as_ref()?)).then_some(p)
    };
    // Plan first (no writes on a dry run).
    let mut herdr = vec![];
    let mut native = vec![];
    for (key, e) in &src.plugins {
        if key != &e.id || !safe_id(&e.id, false) {
            rep.skipped
                .push(format!("{}: invalid plugin id", e.id.escape_debug()));
        } else if current.plugins.contains_key(&e.id) || current.native.contains_key(&e.id) {
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
    for (key, e) in &src.native {
        if key != &e.id || !safe_id(&e.id, true) {
            rep.skipped
                .push(format!("{}: invalid plugin id", e.id.escape_debug()));
        } else if current.plugins.contains_key(&e.id) || current.native.contains_key(&e.id) {
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
        let dest = join_under(
            &dirs.checkouts,
            &Path::new(id).join(format!("import-{}", nonce())),
        )?;
        copy_tree(&src, &dest)?;
        set_tree_writable(&dest, false)?;
        Ok(dest)
    };
    let mut placed_h = vec![];
    for mut e in herdr {
        if e.managed {
            e.root = place(&e.id, &e.root)?;
        }
        // The export's legacy grant and build state are not trusted (a forged entry could carry
        // grant hashes computed for any directory): review and build again.
        e.trust = None;
        e.built = false;
        rep.needs_review.push(e.id.clone());
        placed_h.push(e);
    }
    let mut placed_n = vec![];
    for mut e in native {
        if e.managed {
            e.root = place(&e.id, &e.root)?;
            e.tree_sha256 = crate::herdr::registry::tree_digest(&e.root)?;
        } else {
            e.tree_sha256.clear();
        }
        // The export's consent and build state are not trusted: consent again.
        e.consent = None;
        e.built = false;
        rep.needs_review.push(e.id.clone());
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
        let rel = Path::new(id);
        copy_missing(
            &join_under(&from.join("config"), rel)?,
            &join_under(&dirs.config, rel)?,
            &mut rep,
            id,
        )?;
        copy_missing(
            &join_under(&from.join("state"), rel)?,
            &join_under(&dirs.state, rel)?,
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
        assert!(e.consent.is_none(), "native consent is dropped on import");
        assert!(!e.built);
        assert_eq!(rep.needs_review, vec!["acme.bk"]);
        assert_eq!(
            e.tree_sha256,
            crate::herdr::registry::tree_digest(&e.root).unwrap(),
            "tree digest recomputed from the imported files"
        );
        assert!(b.state_dir("acme.bk").join("db.json").is_file());
        // A second import conflicts and changes nothing.
        let again = import(&b, &out, false).unwrap();
        assert!(again.plugins.is_empty());
        assert_eq!(again.conflicts.len(), 1);
    }

    /// A Herdr entry's legacy grant (here a genuine one for a linked directory that exists on
    /// the importing machine, exactly what a forged export could carry) is not imported: the
    /// plugin is untrusted until reviewed again.
    #[test]
    fn import_drops_herdr_grants() {
        use crate::herdr::registry::Status;
        let t = tempfile::tempdir().unwrap();
        let a = dirs(&t.path().join("a"));
        let src = t.path().join("acme.hd");
        std::fs::create_dir_all(src.join("bin")).unwrap();
        std::fs::write(
            src.join(crate::herdr::MANIFEST_FILE),
            "id = \"acme.hd\"\nversion = \"1.0.0\"\n[[actions]]\nid = \"go\"\ntitle = \"Go\"\ncommand = [\"bin/go\"]\n",
        )
        .unwrap();
        std::fs::write(src.join("bin/go"), "#!/bin/sh\necho go\n").unwrap();
        Registry::update(&a, |r| {
            r.link(&src)?;
            r.trust("acme.hd").map(|_| ())
        })
        .unwrap();
        assert_eq!(
            Registry::load(&a).unwrap().status("acme.hd").unwrap().0,
            Status::Active
        );
        let out = t.path().join("export");
        export(&a, &out).unwrap();
        let b = dirs(&t.path().join("b"));
        let rep = import(&b, &out, false).unwrap();
        assert_eq!(rep.plugins, vec!["acme.hd"]);
        assert_eq!(rep.needs_review, vec!["acme.hd"]);
        let reg = Registry::load(&b).unwrap();
        let e = reg.get("acme.hd").unwrap();
        assert!(e.trust.is_none() && !e.built);
        assert_eq!(reg.status("acme.hd").unwrap().0, Status::Untrusted);
    }

    #[test]
    fn import_refuses_path_traversal_ids() {
        let t = tempfile::tempdir().unwrap();
        let a = dirs(&t.path().join("a"));
        let src = t.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join(super::super::MANIFEST_FILE),
            "id = \"acme.bk\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();
        let staged = stage(&a, &src, local_origin(&src, None)).unwrap();
        Registry::update(&a, |r| r.native_install_staged(&a, staged).map(|_| ())).unwrap();
        let out = t.path().join("export");
        export(&a, &out).unwrap();
        // Forge the id in the exported registry.
        let text = std::fs::read_to_string(out.join("plugins.json")).unwrap();
        let forged = text.replace("acme.bk", "../../x");
        assert_ne!(text, forged);
        std::fs::write(out.join("plugins.json"), forged).unwrap();
        let b = dirs(&t.path().join("b"));
        let rep = import(&b, &out, false).unwrap();
        assert!(rep.plugins.is_empty(), "{rep:?}");
        assert_eq!(rep.skipped.len(), 1);
        assert!(rep.skipped[0].contains("invalid plugin id"));
        assert!(Registry::load(&b).unwrap().native.is_empty());
        assert!(!t.path().join("x").exists());
        assert!(!safe_id("../../x", false) && !safe_id("a/b", false) && !safe_id("a.b/..", true));
        assert!(safe_id("acme.bk", true) && safe_id("reviewr", false));
    }
}
