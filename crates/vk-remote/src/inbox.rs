//! The pane inbox (06 A11.4) and directory drops (A11.2).
//!
//! - [`pack_dir`]: the client packs a dropped directory as a tar stream (regular files and
//!   directories only; symlinks are not followed and special files are skipped), bounded by a
//!   byte budget.
//! - [`unpack_tar`]: the server unpacks it under the inbox with the same rules plus path
//!   checks (no absolute paths, no `..`, no links, nothing outside the destination), files
//!   0600 and directories 0700.
//! - [`sweep`]: the retention sweeper removes inbox entries older than
//!   `paste.inbox_retention` (and abandoned staging files).

use anyhow::{Context, Result, bail};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

/// What [`pack_dir`] skipped (reported to the user, never silently).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PackReport {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    pub skipped_links: u64,
    pub skipped_special: u64,
}

/// Pack directory `dir` into a tar written to `out`, rooted at the directory's own name (so
/// it unpacks as `<inbox>/<hash>/<dirname>/…`). Fails once the file bytes exceed `budget`.
pub fn pack_dir<W: Write>(dir: &Path, out: W, budget: u64) -> Result<PackReport> {
    let meta = std::fs::symlink_metadata(dir).with_context(|| format!("{}", dir.display()))?;
    if !meta.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    let root = dir
        .file_name()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("dir"));
    let mut b = tar::Builder::new(out);
    b.follow_symlinks(false);
    let mut rep = PackReport::default();
    let mut stack = vec![(dir.to_path_buf(), root.clone())];
    b.append_dir(&root, dir)?;
    rep.dirs += 1;
    while let Some((fs_dir, tar_dir)) = stack.pop() {
        let mut entries: Vec<_> = std::fs::read_dir(&fs_dir)?.collect::<Result<_, _>>()?;
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let p = e.path();
            let name = tar_dir.join(e.file_name());
            let m = std::fs::symlink_metadata(&p)?;
            let ft = m.file_type();
            if ft.is_symlink() {
                rep.skipped_links += 1;
            } else if ft.is_dir() {
                b.append_dir(&name, &p)?;
                rep.dirs += 1;
                stack.push((p, name));
            } else if ft.is_file() {
                rep.bytes += m.len();
                if rep.bytes > budget {
                    bail!(
                        "{} is larger than {} bytes (paste.max_auto_bytes)",
                        dir.display(),
                        budget
                    );
                }
                let mut f = std::fs::File::open(&p)?;
                b.append_file(&name, &mut f)?;
                rep.files += 1;
            } else {
                rep.skipped_special += 1;
            }
        }
    }
    b.into_inner()?.flush()?;
    Ok(rep)
}

/// A relative, normal path inside the archive (no root, no `..`, no prefix).
fn safe_rel(p: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::Normal(x) => out.push(x),
            Component::CurDir => {}
            _ => return None,
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

/// Unpack a tar stream into `dest` (created 0700). Regular files (0600) and directories
/// (0700) only; links, devices and fifos are skipped; any entry escaping `dest` aborts the
/// whole unpack. `max_bytes` bounds the total unpacked file bytes. Returns the top-level
/// entry names, sorted (normally the single dropped directory).
pub fn unpack_tar<R: Read>(src: R, dest: &Path, max_bytes: u64) -> Result<Vec<String>> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    std::fs::create_dir_all(dest)?;
    std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o700))?;
    let mut ar = tar::Archive::new(src);
    let mut total = 0u64;
    let mut tops = std::collections::BTreeSet::new();
    for e in ar.entries()? {
        let mut e = e?;
        let raw = e.path()?.into_owned();
        let Some(rel) = safe_rel(&raw) else {
            bail!("refusing archive entry {}", raw.display());
        };
        let kind = e.header().entry_type();
        if !(kind.is_file() || kind.is_dir()) {
            continue; // symlinks, hard links, devices, fifos
        }
        // Every parent must be a real directory we created (no symlink planted earlier).
        let target = dest.join(&rel);
        let mut cur = dest.to_path_buf();
        for c in rel.parent().into_iter().flat_map(|p| p.components()) {
            cur.push(c);
            match std::fs::symlink_metadata(&cur) {
                Ok(m) if m.is_dir() && !m.file_type().is_symlink() => {}
                Ok(_) => bail!("refusing archive entry {}", raw.display()),
                Err(_) => {
                    std::fs::create_dir(&cur)?;
                    std::fs::set_permissions(&cur, std::fs::Permissions::from_mode(0o700))?;
                }
            }
        }
        if let Some(Component::Normal(top)) = rel.components().next() {
            tops.insert(top.to_string_lossy().into_owned());
        }
        if kind.is_dir() {
            match std::fs::symlink_metadata(&target) {
                Ok(m) if m.is_dir() && !m.file_type().is_symlink() => {}
                Ok(_) => bail!("refusing archive entry {}", raw.display()),
                Err(_) => std::fs::create_dir(&target)?,
            }
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700))?;
            continue;
        }
        let size = e.header().size()?;
        total += size;
        if total > max_bytes {
            bail!("archive unpacks to more than {max_bytes} bytes");
        }
        if std::fs::symlink_metadata(&target).is_ok() {
            bail!("duplicate archive entry {}", raw.display());
        }
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&target)?;
        let n = std::io::copy(&mut (&mut e).take(size), &mut f)?;
        if n != size {
            bail!("truncated archive entry {}", raw.display());
        }
    }
    Ok(tops.into_iter().collect())
}

/// What a [`sweep`] removed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SweepReport {
    pub removed: u64,
    pub bytes: u64,
}

fn tree_size(p: &Path) -> u64 {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => std::fs::read_dir(p)
            .map(|rd| rd.flatten().map(|e| tree_size(&e.path())).sum())
            .unwrap_or(0),
        Ok(m) => m.len(),
        Err(_) => 0,
    }
}

/// Remove inbox entries (`<inbox>/<hash12>/…` and stale `.incoming` staging files) whose
/// modification time is older than `retention` at `now`. `retention` zero keeps everything.
/// Only direct children of `inbox` are considered; symlinks are removed, never followed.
pub fn sweep(inbox: &Path, retention: Duration, now: SystemTime) -> SweepReport {
    let mut rep = SweepReport::default();
    if retention.is_zero() {
        return rep;
    }
    let Some(cutoff) = now.checked_sub(retention) else {
        return rep;
    };
    let old = |p: &Path| {
        std::fs::symlink_metadata(p)
            .and_then(|m| m.modified())
            .is_ok_and(|t| t < cutoff)
    };
    let Ok(rd) = std::fs::read_dir(inbox) else {
        return rep;
    };
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if name == ".incoming" {
            // Staging files of uploads abandoned without blob.abort.
            if let Ok(rd) = std::fs::read_dir(&p) {
                for s in rd.flatten() {
                    if old(&s.path()) {
                        rep.bytes += tree_size(&s.path());
                        if std::fs::remove_file(s.path()).is_ok() {
                            rep.removed += 1;
                        }
                    }
                }
            }
            continue;
        }
        if !old(&p) {
            continue;
        }
        rep.bytes += tree_size(&p);
        let ok = match std::fs::symlink_metadata(&p) {
            Ok(m) if m.is_dir() => std::fs::remove_dir_all(&p).is_ok(),
            Ok(_) => std::fs::remove_file(&p).is_ok(),
            Err(_) => false,
        };
        if ok {
            rep.removed += 1;
        }
    }
    rep
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn pack_unpack_roundtrip_skips_links_and_keeps_names() {
        let src = tempfile::tempdir().unwrap();
        let d = src.path().join("My Project ✓");
        std::fs::create_dir_all(d.join("sub dir")).unwrap();
        std::fs::write(d.join("a.txt"), b"hello").unwrap();
        std::fs::write(d.join("sub dir/b.bin"), vec![7u8; 70_000]).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", d.join("link")).unwrap();
        let mut tar = Vec::new();
        let rep = pack_dir(&d, &mut tar, 1 << 20).unwrap();
        assert_eq!((rep.files, rep.dirs, rep.skipped_links), (2, 2, 1));
        assert_eq!(rep.bytes, 70_005);
        let dst = tempfile::tempdir().unwrap();
        let out = dst.path().join("3f9a1c0b2e7d");
        let tops = unpack_tar(&tar[..], &out, 1 << 20).unwrap();
        assert_eq!(tops, vec!["My Project ✓".to_string()]);
        let root = out.join("My Project ✓");
        assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), b"hello");
        assert_eq!(
            std::fs::read(root.join("sub dir/b.bin")).unwrap().len(),
            70_000
        );
        assert!(std::fs::symlink_metadata(root.join("link")).is_err());
        assert_eq!(mode(&root.join("a.txt")), 0o600);
        assert_eq!(mode(&root.join("sub dir")), 0o700);
        assert_eq!(mode(&out), 0o700);
        // Over budget refuses to pack.
        assert!(pack_dir(&d, &mut Vec::new(), 1000).is_err());
        // And the unpack cap is enforced too.
        assert!(unpack_tar(&tar[..], &dst.path().join("x"), 1000).is_err());
    }

    fn raw_tar(entries: &[(&str, tar::EntryType, &[u8], Option<&str>)]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (name, kind, data, link) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(*kind);
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            if let Some(l) = link {
                h.set_link_name(l).unwrap();
            }
            // Write the name bytes directly so `..` and absolute names survive.
            let bytes = name.as_bytes();
            h.as_old_mut().name[..bytes.len()].copy_from_slice(bytes);
            h.set_cksum();
            b.append(&h, *data).unwrap();
        }
        b.into_inner().unwrap()
    }

    #[test]
    fn hostile_archives_are_refused() {
        let dst = tempfile::tempdir().unwrap();
        for (i, bad) in [
            raw_tar(&[("../escape.txt", tar::EntryType::Regular, b"x", None)]),
            raw_tar(&[("/abs.txt", tar::EntryType::Regular, b"x", None)]),
            raw_tar(&[("d/../../e", tar::EntryType::Regular, b"x", None)]),
        ]
        .into_iter()
        .enumerate()
        {
            let out = dst.path().join(format!("o{i}"));
            assert!(unpack_tar(&bad[..], &out, 1 << 20).is_err(), "case {i}");
        }
        assert!(!dst.path().join("escape.txt").exists());
        // A symlink entry is skipped, so a later file "through" it cannot escape.
        let t = raw_tar(&[
            ("d/link", tar::EntryType::Symlink, b"", Some("/tmp")),
            ("d/link/pwned", tar::EntryType::Regular, b"x", None),
        ]);
        let out = dst.path().join("sym");
        let r = unpack_tar(&t[..], &out, 1 << 20);
        assert!(
            std::fs::symlink_metadata(out.join("d/link"))
                .map(|m| !m.file_type().is_symlink())
                .unwrap_or(true)
        );
        assert!(!Path::new("/tmp/pwned").exists() || r.is_err());
        // Device / fifo entries are skipped.
        let t = raw_tar(&[
            ("ok.txt", tar::EntryType::Regular, b"fine", None),
            ("fifo", tar::EntryType::Fifo, b"", None),
        ]);
        let out = dst.path().join("fifo");
        unpack_tar(&t[..], &out, 1 << 20).unwrap();
        assert!(out.join("ok.txt").exists() && !out.join("fifo").exists());
    }

    #[test]
    fn sweep_removes_only_expired_entries() {
        let inbox = tempfile::tempdir().unwrap();
        let old = inbox.path().join("aaaaaaaaaaaa");
        let new = inbox.path().join("bbbbbbbbbbbb");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(old.join("shot.png"), vec![1u8; 100]).unwrap();
        std::fs::write(new.join("shot.png"), vec![1u8; 100]).unwrap();
        std::fs::create_dir_all(inbox.path().join(".incoming")).unwrap();
        std::fs::write(inbox.path().join(".incoming/up-1"), b"partial").unwrap();
        let now = SystemTime::now();
        // Nothing is older than 14 days yet.
        assert_eq!(
            sweep(inbox.path(), Duration::from_secs(14 * 86400), now),
            SweepReport::default()
        );
        // Make `old` and the staging file look 15 days old.
        let past = now - Duration::from_secs(15 * 86400);
        for p in [&old, &inbox.path().join(".incoming/up-1")] {
            let f = std::fs::File::open(p).unwrap();
            f.set_modified(past).unwrap();
        }
        let rep = sweep(inbox.path(), Duration::from_secs(14 * 86400), now);
        assert_eq!(rep.removed, 2);
        assert_eq!(rep.bytes, 107);
        assert!(!old.exists() && new.exists());
        assert!(inbox.path().join(".incoming").exists());
        // Zero retention keeps everything.
        let f = std::fs::File::open(&new).unwrap();
        f.set_modified(past).unwrap();
        assert_eq!(sweep(inbox.path(), Duration::ZERO, now).removed, 0);
        assert!(new.exists());
    }
}
