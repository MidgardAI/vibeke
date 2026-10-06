//! Symlink-safe filesystem helpers for directories a contained process can write to (13 §5,
//! §8): pane private dirs and projected harness homes live under Vibeke's sandbox root, and the
//! box may replace any entry it can write with a symlink to a host path. Everything the server
//! later creates, writes or grants under such a dir goes through these helpers, which never
//! follow a symlink and verify containment under the root Vibeke created.

use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

fn refused(what: &str, p: &Path) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        format!("{what}: {} (refusing to follow it)", p.display()),
    )
}

/// `root/rel`, creating missing directories (0700) one component at a time. Every existing
/// component must be a real directory (lstat): a symlink anywhere below `root` is an error, so
/// the result is always physically inside `root`. `rel` must be relative without `..`.
pub fn ensure_dir_under(root: &Path, rel: &Path) -> std::io::Result<PathBuf> {
    let md = std::fs::symlink_metadata(root)?;
    if md.file_type().is_symlink() || !md.is_dir() {
        return Err(refused("sandbox root is not a directory", root));
    }
    let mut cur = root.to_path_buf();
    for c in rel.components() {
        let Component::Normal(name) = c else {
            return Err(refused("unexpected path component", rel));
        };
        cur.push(name);
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => return Err(refused("symlink", &cur)),
            Ok(m) if !m.is_dir() => return Err(refused("not a directory", &cur)),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&cur)?;
                std::fs::set_permissions(&cur, std::fs::Permissions::from_mode(0o700))?;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(cur)
}

/// Is `path` physically inside `root` (no symlink in any component from `root` down, the
/// final component included)? Missing components count as not contained.
pub fn contained_no_symlink(root: &Path, path: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(root) else {
        return false;
    };
    if !std::fs::symlink_metadata(root).is_ok_and(|m| !m.file_type().is_symlink()) {
        return false;
    }
    let mut cur = root.to_path_buf();
    for c in rel.components() {
        let Component::Normal(name) = c else {
            return false;
        };
        cur.push(name);
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if !m.file_type().is_symlink() => {}
            _ => return false,
        }
    }
    true
}

/// Is the last component of `path` a real file or directory (not a symlink) whose parent
/// resolves to the same place lexically and physically? Used for grants outside the sandbox
/// root (omp's host session dir), where only the final component is writable by the box.
pub fn final_component_real(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(m) if !m.file_type().is_symlink() => {}
        _ => return false,
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => parent
            .canonicalize()
            .ok()
            .zip(path.canonicalize().ok())
            .is_some_and(|(pc, c)| c == pc.join(name)),
        _ => false,
    }
}

/// Write `bytes` to `path` with `mode`, never following a symlink: an existing entry (file or
/// symlink) is removed first and the new file is created with `O_EXCL | O_NOFOLLOW`. The parent
/// must already be a verified directory.
pub fn write_nofollow(path: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    if let Ok(m) = std::fs::symlink_metadata(path) {
        if m.is_dir() {
            return Err(refused("is a directory", path));
        }
        if !m.file_type().is_symlink() {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::remove_file(path)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)?;
    drop(f);
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_dir_refuses_symlinks_and_stays_inside() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        let outside = root.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let sbx = root.join("sbx");
        std::fs::create_dir_all(&sbx).unwrap();
        let d = ensure_dir_under(&sbx, Path::new("home/claude")).unwrap();
        assert_eq!(d, sbx.join("home/claude"));
        assert!(contained_no_symlink(&sbx, &d));
        // The box replaces its projected home with a symlink to a host dir.
        std::fs::remove_dir(&d).unwrap();
        std::os::unix::fs::symlink(&outside, &d).unwrap();
        assert!(ensure_dir_under(&sbx, Path::new("home/claude")).is_err());
        assert!(!contained_no_symlink(&sbx, &d));
        assert!(!contained_no_symlink(&sbx, &d.join("x")));
        assert!(!contained_no_symlink(&sbx, &outside));
        assert!(ensure_dir_under(&sbx, Path::new("../x")).is_err());
        // A symlinked root itself is refused too.
        let link_root = root.join("link-root");
        std::os::unix::fs::symlink(&sbx, &link_root).unwrap();
        assert!(ensure_dir_under(&link_root, Path::new("a")).is_err());
        assert!(!final_component_real(&d));
        assert!(final_component_real(&outside));
    }

    #[test]
    fn write_nofollow_replaces_a_symlink_instead_of_writing_through_it() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        let victim = root.join("victim");
        std::fs::write(&victim, "host file").unwrap();
        let p = root.join("profile.sb");
        std::os::unix::fs::symlink(&victim, &p).unwrap();
        write_nofollow(&p, b"new", 0o400).unwrap();
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "host file");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "new");
        assert!(
            !std::fs::symlink_metadata(&p)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o400
        );
        // Overwriting a read-only regular file works.
        write_nofollow(&p, b"again", 0o600).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "again");
    }
}
