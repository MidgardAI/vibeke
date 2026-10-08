//! Symlink-safe filesystem helpers for directories a contained process can write to (13 §5,
//! §8): pane private dirs and projected harness homes live under Vibeke's sandbox root, and the
//! box may replace any entry it can write with a symlink to a host path. Everything the server
//! later creates, writes or grants under such a dir goes through these helpers, which never
//! follow a symlink and verify containment under the root Vibeke created.

use rustix::fs::{self as rfs, Mode, OFlags};
use rustix::io::Errno;
use std::ffi::OsStr;
use std::io::Write as _;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

fn refused(what: &str, p: &Path) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        format!("{what}: {} (refusing to follow it)", p.display()),
    )
}

fn dir_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

/// Open the directory `name` relative to `dir` without following a symlink in that component.
fn open_dir_at<Fd: AsFd>(dir: Fd, name: &OsStr, shown: &Path) -> std::io::Result<OwnedFd> {
    match rfs::openat(dir, name, dir_flags(), Mode::empty()) {
        Ok(fd) => Ok(fd),
        Err(e) if e == Errno::LOOP || e == Errno::NOTDIR => {
            Err(refused("symlink or not a directory", shown))
        }
        Err(e) => Err(e.into()),
    }
}

/// Open `root` (its last component must be a real directory, not a symlink) and walk `rel`
/// from it with `openat(O_DIRECTORY | O_NOFOLLOW)` for every component, creating missing
/// directories (0700) with `mkdirat` relative to the parent's fd. A symlink anywhere below
/// `root` is an error, and nothing is ever resolved by path after the root was opened, so a
/// concurrent swap of a component for a symlink cannot redirect the walk. `rel` must be
/// relative without `..`. Returns the final directory's fd and its (lexical) path.
pub fn open_dir_under(root: &Path, rel: &Path) -> std::io::Result<(OwnedFd, PathBuf)> {
    let mut dir = open_dir_at(rfs::CWD, root.as_os_str(), root)?;
    let mut cur = root.to_path_buf();
    for c in rel.components() {
        let Component::Normal(name) = c else {
            return Err(refused("unexpected path component", rel));
        };
        cur.push(name);
        let created = match rfs::mkdirat(&dir, name, Mode::RWXU) {
            Ok(()) => true,
            Err(e) if e == Errno::EXIST => false,
            Err(e) => return Err(e.into()),
        };
        dir = open_dir_at(&dir, name, &cur)?;
        if created {
            rfs::fchmod(&dir, Mode::RWXU)?;
        }
    }
    Ok((dir, cur))
}

/// `root/rel`, creating missing directories (0700) one component at a time (see
/// [`open_dir_under`]): the result is always physically inside `root`.
pub fn ensure_dir_under(root: &Path, rel: &Path) -> std::io::Result<PathBuf> {
    open_dir_under(root, rel).map(|(_, p)| p)
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

/// Write `bytes` to `path` with `mode`, never following a symlink: the parent is opened with
/// `O_DIRECTORY | O_NOFOLLOW`, an existing entry (file or symlink) is unlinked relative to that
/// fd, the new file is created with `O_EXCL | O_NOFOLLOW` and its mode set with `fchmod`, so no
/// step resolves the final component by path. The parent must already be a verified directory
/// (see [`ensure_dir_under`]).
pub fn write_nofollow(path: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(refused("not a file path", path));
    };
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let dir = open_dir_at(rfs::CWD, parent.as_os_str(), parent)?;
    write_nofollow_at(&dir, name, path, bytes, mode)
}

/// [`write_nofollow`] relative to an already opened directory fd.
pub fn write_nofollow_at<Fd: AsFd>(
    dir: Fd,
    name: &OsStr,
    shown: &Path,
    bytes: &[u8],
    mode: u32,
) -> std::io::Result<()> {
    let dir = dir.as_fd();
    match rfs::unlinkat(dir, name, rfs::AtFlags::empty()) {
        Ok(()) => {}
        Err(e) if e == Errno::NOENT => {}
        Err(e) if e == Errno::ISDIR || e == Errno::PERM => {
            // unlink(2) of a directory: EISDIR (Linux) / EPERM (macOS).
            return Err(refused("is a directory", shown));
        }
        Err(e) => return Err(e.into()),
    }
    let fd = rfs::openat(
        dir,
        name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )?;
    let mut f = std::fs::File::from(fd);
    f.write_all(bytes)?;
    f.set_permissions(std::fs::Permissions::from_mode(mode))
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
    fn walk_never_follows_an_intermediate_symlink() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        let sbx = root.join("sbx");
        let outside = root.join("outside");
        std::fs::create_dir_all(sbx.join("a")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        // A middle component swapped for a symlink: nothing is created through it.
        std::os::unix::fs::symlink(&outside, sbx.join("a/b")).unwrap();
        assert!(ensure_dir_under(&sbx, Path::new("a/b/c")).is_err());
        assert!(!outside.join("c").exists());
        // Created directories are 0700 and the fd refers to the final directory.
        let (fd, p) = open_dir_under(&sbx, Path::new("x/y")).unwrap();
        assert_eq!(p, sbx.join("x/y"));
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o700
        );
        write_nofollow_at(&fd, OsStr::new("f"), &p.join("f"), b"hi", 0o600).unwrap();
        assert_eq!(std::fs::read_to_string(p.join("f")).unwrap(), "hi");
        // A symlinked parent is refused by write_nofollow.
        let link = sbx.join("link");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert!(write_nofollow(&link.join("f"), b"x", 0o600).is_err());
        assert!(!outside.join("f").exists());
        // A directory in the target's place is refused, not removed.
        std::fs::create_dir_all(sbx.join("d")).unwrap();
        assert!(write_nofollow(&sbx.join("d"), b"x", 0o600).is_err());
        assert!(sbx.join("d").is_dir());
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
