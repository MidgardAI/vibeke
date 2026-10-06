//! Copy-on-write cloning of files and directory trees (05 §5): `clonefile(2)`
//! on macOS (APFS), `ioctl(FICLONE)` per file on Linux (btrfs/xfs/bcachefs),
//! falling back to a plain copy anywhere else.

use std::fs;
use std::io;
use std::path::Path;

/// How a tree ended up at its destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CloneMethod {
    /// Every file is a copy-on-write clone (~0 bytes).
    CopyOnWrite,
    /// Some files cloned, some copied.
    Mixed,
    /// Plain copy: the filesystem has no reflinks.
    Copy,
}

#[derive(Default)]
struct Tally {
    cloned: u64,
    copied: u64,
}

impl Tally {
    fn method(&self) -> CloneMethod {
        match (self.cloned, self.copied) {
            (_, 0) => CloneMethod::CopyOnWrite,
            (0, _) => CloneMethod::Copy,
            _ => CloneMethod::Mixed,
        }
    }
}

/// Clone `src` (file, symlink or directory) to the not-yet-existing `dst`,
/// creating parent directories. Never overwrites: an existing `dst` is an
/// `AlreadyExists` error.
pub fn clone_path(src: &Path, dst: &Path) -> io::Result<CloneMethod> {
    if fs::symlink_metadata(dst).is_ok() {
        return Err(io::ErrorKind::AlreadyExists.into());
    }
    if let Some(p) = dst.parent() {
        fs::create_dir_all(p)?;
    }
    // Whole-tree clone in one syscall where the platform has it.
    if native_clone(src, dst).is_ok() {
        return Ok(CloneMethod::CopyOnWrite);
    }
    let mut t = Tally::default();
    walk(src, dst, &mut t)?;
    Ok(t.method())
}

/// Can `dir`'s filesystem clone a file from `src_file`? Probes with a real
/// clone of one existing file into `dir`, which is removed again.
pub fn cow_available(src_file: &Path, dir: &Path) -> bool {
    let probe = dir.join(format!(".vibeke-cow-probe-{}", std::process::id()));
    let ok = native_clone(src_file, &probe).is_ok();
    let _ = fs::remove_file(&probe);
    ok
}

fn walk(src: &Path, dst: &Path, t: &mut Tally) -> io::Result<()> {
    let meta = fs::symlink_metadata(src)?;
    let ft = meta.file_type();
    if ft.is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(src)?, dst)?;
    } else if ft.is_dir() {
        fs::create_dir(dst)?;
        for e in fs::read_dir(src)? {
            let e = e?;
            walk(&e.path(), &dst.join(e.file_name()), t)?;
        }
        fs::set_permissions(dst, meta.permissions())?;
    } else if ft.is_file() {
        if file_clone(src, dst).is_ok() {
            t.cloned += 1;
        } else {
            let _ = fs::remove_file(dst);
            fs::copy(src, dst)?;
            t.copied += 1;
        }
    }
    // Sockets, fifos and devices are skipped.
    Ok(())
}

#[cfg(target_os = "macos")]
fn native_clone(src: &Path, dst: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let s = CString::new(src.as_os_str().as_bytes()).map_err(io::Error::other)?;
    let d = CString::new(dst.as_os_str().as_bytes()).map_err(io::Error::other)?;
    // SAFETY: valid NUL-terminated paths; CLONE_NOFOLLOW (1) keeps symlinks as links.
    let r = unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), 1) };
    if r == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "macos")]
fn file_clone(src: &Path, dst: &Path) -> io::Result<()> {
    native_clone(src, dst)
}

#[cfg(target_os = "linux")]
fn native_clone(src: &Path, dst: &Path) -> io::Result<()> {
    // FICLONE works on regular files only; directories go through `walk`.
    if !fs::symlink_metadata(src)?.is_file() {
        return Err(io::ErrorKind::Unsupported.into());
    }
    file_clone(src, dst)
}

#[cfg(target_os = "linux")]
fn file_clone(src: &Path, dst: &Path) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // _IOW(0x94, 9, int)
    const FICLONE: libc::c_ulong = 0x4004_9409;
    let s = fs::File::open(src)?;
    let meta = s.metadata()?;
    let d = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dst)?;
    // SAFETY: both fds are open and owned for the duration of the call.
    let r = unsafe { libc::ioctl(d.as_raw_fd(), FICLONE as _, s.as_raw_fd()) };
    if r != 0 {
        let e = io::Error::last_os_error();
        drop(d);
        let _ = fs::remove_file(dst);
        return Err(e);
    }
    fs::set_permissions(dst, meta.permissions())?;
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn native_clone(_: &Path, _: &Path) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn file_clone(_: &Path, _: &Path) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(root: &Path) {
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("a/x.txt"), "x").unwrap();
        fs::write(root.join("a/b/y.txt"), "yy").unwrap();
        std::os::unix::fs::symlink("x.txt", root.join("a/link")).unwrap();
    }

    #[test]
    fn clones_a_tree_independently() {
        let t = tempfile::tempdir().unwrap();
        tree(&t.path().join("src"));
        let m = clone_path(&t.path().join("src/a"), &t.path().join("dst/a")).unwrap();
        // CoW where the temp dir's filesystem supports it, copy elsewhere.
        assert!(matches!(
            m,
            CloneMethod::CopyOnWrite | CloneMethod::Mixed | CloneMethod::Copy
        ));
        assert_eq!(
            fs::read_to_string(t.path().join("dst/a/b/y.txt")).unwrap(),
            "yy"
        );
        assert_eq!(
            fs::read_link(t.path().join("dst/a/link")).unwrap(),
            Path::new("x.txt")
        );
        // Writing the clone leaves the source alone.
        fs::write(t.path().join("dst/a/x.txt"), "changed").unwrap();
        assert_eq!(
            fs::read_to_string(t.path().join("src/a/x.txt")).unwrap(),
            "x"
        );
    }

    #[test]
    fn never_overwrites_and_falls_back_to_copy() {
        let t = tempfile::tempdir().unwrap();
        tree(&t.path().join("src"));
        fs::create_dir_all(t.path().join("dst/a")).unwrap();
        let e = clone_path(&t.path().join("src/a"), &t.path().join("dst/a")).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        // The copy path itself (what a non-reflink filesystem gets) works.
        let mut tally = Tally::default();
        walk(&t.path().join("src/a"), &t.path().join("copy"), &mut tally).unwrap();
        assert_eq!(
            fs::read_to_string(t.path().join("copy/x.txt")).unwrap(),
            "x"
        );
        assert_eq!(tally.cloned + tally.copied, 2);
    }

    #[test]
    fn cow_probe_leaves_nothing_behind() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("f"), "1").unwrap();
        let _ = cow_available(&t.path().join("f"), t.path());
        let names: Vec<_> = fs::read_dir(t.path()).unwrap().flatten().collect();
        assert_eq!(names.len(), 1);
    }
}
