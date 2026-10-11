//! Private git object stores of sandboxed boxes (13 §6).
//!
//! A `sandbox`-level box works in a host worktree, but the repository's shared object store is
//! read-only inside it: a box that could write `.git/objects` could also delete or rewrite the
//! objects every other branch of the user's repository needs. Git in the box therefore writes
//! new objects to a private directory (`GIT_OBJECT_DIRECTORY`) and reads the host's objects as
//! an alternate.
//!
//! Host-side git needs those objects too (review, diff, merge, push, handoff, finish). The
//! server registers each box's store with [`register_box_objects`]; from then on every command
//! that goes through [`crate::harden_command`] in that repository first **imports** the
//! box's new objects into the repository's own store, and [`release_box_objects`] imports
//! everything once more before the box's directory is deleted. Imported objects never depend
//! on the box's directory, so deleting it cannot corrupt the repository.
//!
//! The import never trusts the box's files: each loose object and pack is opened without
//! following a symlink at any component and copied into a staging directory the box cannot
//! reach, and git re-hashes every object while it indexes the copy (`index-pack`), so a file
//! whose content does not match its name is stored under its real name or refused.

use std::collections::HashSet;
use std::io::{Read as _, Write as _};
use std::os::fd::{FromRawFd as _, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};

/// Upper bound on the bytes one import copies out of a box's store.
const MAX_IMPORT_BYTES: u64 = 8 * 1024 * 1024 * 1024;

#[derive(Debug)]
struct Store {
    /// The repository's common git dir (canonical).
    common: PathBuf,
    /// The box's private object directory.
    objects: PathBuf,
    /// Files (`ab/cdef…`, `pack/pack-…`) already imported or found broken.
    seen: HashSet<String>,
}

fn stores() -> &'static Mutex<Vec<Store>> {
    static S: OnceLock<Mutex<Vec<Store>>> = OnceLock::new();
    S.get_or_init(Mutex::default)
}

/// One import at a time: two commands must not stage into the same directory.
fn import_lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(Mutex::default)
}

fn canon(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// The common git dir of the repository `dir` belongs to, found by reading `.git`,
/// `gitdir:` links and `commondir` files (no git process). `None` outside a repository.
pub(crate) fn common_dir_of(dir: &Path) -> Option<PathBuf> {
    let start = canon(dir);
    let mut gitdir = None;
    for anc in start.ancestors() {
        let dot = anc.join(".git");
        match std::fs::symlink_metadata(&dot) {
            Ok(m) if m.is_dir() => {
                gitdir = Some(canon(&dot));
                break;
            }
            Ok(m) if m.is_file() => {
                let text = std::fs::read_to_string(&dot).ok()?;
                let target = text.lines().next()?.strip_prefix("gitdir:")?.trim();
                let p = Path::new(target);
                gitdir = Some(canon(&if p.is_absolute() {
                    p.to_path_buf()
                } else {
                    anc.join(p)
                }));
                break;
            }
            _ => {}
        }
        // `dir` is a git dir itself (a bare repository or `<repo>/.git`).
        if anc.join("HEAD").is_file() && anc.join("objects").is_dir() {
            gitdir = Some(anc.to_path_buf());
            break;
        }
    }
    let gitdir = gitdir?;
    match std::fs::read_to_string(gitdir.join("commondir")) {
        Ok(c) => {
            let p = Path::new(c.trim());
            Some(canon(&if p.is_absolute() {
                p.to_path_buf()
            } else {
                gitdir.join(p)
            }))
        }
        Err(_) => Some(gitdir),
    }
}

/// Register the private object store `objects` of a box that works in `checkout`. Host-side
/// git in the same repository imports the store's new objects before it runs.
pub fn register_box_objects(checkout: &Path, objects: &Path) {
    let Some(common) = common_dir_of(checkout) else {
        return;
    };
    let objects = canon(objects);
    let mut s = stores().lock().unwrap();
    if s.iter().any(|x| x.objects == objects) {
        return;
    }
    s.push(Store {
        common,
        objects,
        seen: HashSet::new(),
    });
}

/// Import every object of the store at `objects` (the box is going away) and forget the
/// store. Returns how many objects were new to the repository. On failure the store stays
/// registered, so the next host git command in the repository tries again; the caller must
/// keep the directory.
pub fn release_box_objects(objects: &Path) -> Result<usize, String> {
    let objects = canon(objects);
    let common = {
        let s = stores().lock().unwrap();
        s.iter()
            .find(|x| x.objects == objects)
            .map(|x| x.common.clone())
    };
    let Some(common) = common else {
        return Ok(0);
    };
    let r = {
        let _g = import_lock().lock().unwrap_or_else(|e| e.into_inner());
        import(&common, &objects, &HashSet::new()).map(|(n, _)| n)
    };
    if r.is_ok() {
        stores().lock().unwrap().retain(|x| x.objects != objects);
    }
    r
}

/// Is any box store registered (cheap check before looking at `dir`)?
fn any_registered() -> bool {
    !stores().lock().unwrap().is_empty()
}

/// Import the new objects of every box store registered for the repository of `dir` (best
/// effort: a failure leaves host git to report a missing object). [`crate::harden_command`]
/// calls this; code that runs git in a task checkout without it (handoff export) calls it
/// first.
pub fn import_box_objects(dir: &Path) {
    if !any_registered() {
        return;
    }
    let Some(common) = common_dir_of(dir) else {
        return;
    };
    let todo: Vec<(PathBuf, HashSet<String>)> = stores()
        .lock()
        .unwrap()
        .iter()
        .filter(|x| x.common == common)
        .map(|x| (x.objects.clone(), x.seen.clone()))
        .collect();
    if todo.is_empty() {
        return;
    }
    let _g = import_lock().lock().unwrap_or_else(|e| e.into_inner());
    for (objects, seen) in todo {
        // A failed import marks its files as seen as well: the box's files do not change
        // under their names, so retrying the same broken file on every git command only
        // slows the host down.
        let files = match import(&common, &objects, &seen) {
            Ok((_, files)) => files,
            Err(e) => {
                eprintln!(
                    "vibeke: importing objects of box store {}: {e}",
                    objects.display()
                );
                candidates(&objects, &seen)
            }
        };
        let mut s = stores().lock().unwrap();
        if let Some(st) = s.iter_mut().find(|x| x.objects == objects) {
            st.seen.extend(files);
        }
    }
}

fn is_hex(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Files of the store not in `seen`: loose objects (`ab/<38 or 62 hex>`) and complete packs
/// (`pack/pack-<hex>` with both `.pack` and `.idx`; git writes the index last).
fn candidates(objects: &Path, seen: &HashSet<String>) -> Vec<String> {
    let mut v = Vec::new();
    let Ok(rd) = std::fs::read_dir(objects) else {
        return v;
    };
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if n.len() != 2 || !is_hex(&n) {
            continue;
        }
        let Ok(sub) = std::fs::read_dir(e.path()) else {
            continue;
        };
        for f in sub.flatten() {
            let f = f.file_name().to_string_lossy().into_owned();
            if (f.len() == 38 || f.len() == 62) && is_hex(&f) {
                let rel = format!("{n}/{f}");
                if !seen.contains(&rel) {
                    v.push(rel);
                }
            }
        }
    }
    if let Ok(rd) = std::fs::read_dir(objects.join("pack")) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            let Some(stem) = n.strip_suffix(".idx") else {
                continue;
            };
            if !stem.strip_prefix("pack-").is_some_and(is_hex) {
                continue;
            }
            let rel = format!("pack/{stem}");
            if !seen.contains(&rel)
                && std::fs::symlink_metadata(objects.join("pack").join(format!("{stem}.pack")))
                    .is_ok()
            {
                v.push(rel);
            }
        }
    }
    v.sort();
    v
}

fn open_at(dir: Option<&OwnedFd>, name: &[u8], flags: libc::c_int) -> std::io::Result<OwnedFd> {
    use std::os::fd::AsRawFd as _;
    let c = std::ffi::CString::new(name)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in path"))?;
    let base = dir.map(|d| d.as_raw_fd()).unwrap_or(libc::AT_FDCWD);
    // SAFETY: `c` is NUL-terminated and `base` is AT_FDCWD or an fd we own.
    let fd = unsafe { libc::openat(base, c.as_ptr(), flags | libc::O_CLOEXEC | libc::O_NOFOLLOW) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: openat returned a fresh descriptor we own.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Copy `<objects>/<sub>/<name>` to `dst` without following a symlink anywhere below (and
/// including) `objects`; only regular files are copied, and at most `budget` bytes in total.
fn copy_nofollow(
    objects: &Path,
    sub: &str,
    name: &str,
    dst: &Path,
    budget: &mut u64,
) -> std::io::Result<()> {
    let refused = |what: &str| {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("{sub}/{name}: {what}"),
        )
    };
    let root = open_at(
        None,
        objects.as_os_str().as_bytes(),
        libc::O_RDONLY | libc::O_DIRECTORY,
    )?;
    let dir = open_at(
        Some(&root),
        sub.as_bytes(),
        libc::O_RDONLY | libc::O_DIRECTORY,
    )?;
    let f = std::fs::File::from(open_at(
        Some(&dir),
        name.as_bytes(),
        libc::O_RDONLY | libc::O_NONBLOCK,
    )?);
    let md = f.metadata()?;
    if !md.is_file() {
        return Err(refused("not a regular file"));
    }
    if md.len() > *budget {
        return Err(refused("the box's object store is too large to import"));
    }
    let mut out = std::fs::File::create(dst)?;
    let n = std::io::copy(&mut f.take(*budget), &mut out)?;
    *budget = budget.saturating_sub(n);
    Ok(())
}

fn git(common: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("--git-dir")
        .arg(common)
        .args(crate::HOST_HARDEN)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C");
    c
}

fn fail(what: &str, out: &std::process::Output) -> String {
    format!(
        "git {what} failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    )
}

/// Run `cmd` with `input` on stdin (written from a thread, so a full stdout pipe cannot
/// deadlock the writer) and collect its output.
fn run_with_input(mut cmd: Command, input: Vec<u8>) -> std::io::Result<std::process::Output> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take();
    let w = std::thread::spawn(move || {
        if let Some(s) = stdin.as_mut() {
            let _ = s.write_all(&input);
        }
    });
    let out = child.wait_with_output();
    let _ = w.join();
    out
}

/// Copy the store's new files (not in `seen`) into a staging dir and import the objects the
/// repository lacks. Returns (objects imported, files handled).
fn import(
    common: &Path,
    objects: &Path,
    seen: &HashSet<String>,
) -> Result<(usize, Vec<String>), String> {
    let files = candidates(objects, seen);
    if files.is_empty() {
        return Ok((0, files));
    }
    // Next to the store, in the box's root: the box can write neither (13 §6).
    let staging = match (objects.parent(), objects.file_name()) {
        (Some(p), Some(n)) => p.join(format!("{}-import", n.to_string_lossy())),
        _ => return Err("bad object store path".into()),
    };
    let _ = std::fs::remove_dir_all(&staging);
    {
        use std::os::unix::fs::DirBuilderExt as _;
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&staging)
            .map_err(|e| format!("{}: {e}", staging.display()))?;
    }
    let r = stage_and_import(common, objects, &staging, &files);
    let _ = std::fs::remove_dir_all(&staging);
    r.map(|n| (n, files))
}

fn stage_and_import(
    common: &Path,
    objects: &Path,
    staging: &Path,
    files: &[String],
) -> Result<usize, String> {
    let mut budget = MAX_IMPORT_BYTES;
    for rel in files {
        let (sub, name) = rel.split_once('/').ok_or("bad name")?;
        let dst_dir = staging.join(sub);
        std::fs::create_dir_all(&dst_dir).map_err(|e| e.to_string())?;
        let names: Vec<String> = if sub == "pack" {
            vec![format!("{name}.pack"), format!("{name}.idx")]
        } else {
            vec![name.to_string()]
        };
        for n in names {
            match copy_nofollow(objects, sub, &n, &dst_dir.join(&n), &mut budget) {
                Ok(()) => {}
                // Gone since the listing (git replaced loose objects by a pack): skip it.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                // A symlink (file or fan-out dir) or a special file: never imported.
                Err(e) if matches!(e.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) => {}
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                    if e.to_string().contains("too large") {
                        return Err(e.to_string());
                    }
                }
                Err(e) => return Err(format!("{rel}: {e}")),
            }
        }
    }
    // The object names in the staged copy (names only: nothing is read yet).
    let out = git(common)
        .env("GIT_OBJECT_DIRECTORY", staging)
        .args([
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname)",
        ])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(fail("cat-file --batch-all-objects", &out));
    }
    let staged = out.stdout;
    if staged.is_empty() {
        return Ok(0);
    }
    // Which of them the repository lacks.
    let mut check = git(common);
    check.args(["cat-file", "--batch-check=%(objectname)"]);
    let out = run_with_input(check, staged).map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(fail("cat-file --batch-check", &out));
    }
    let missing: Vec<&str> = std::str::from_utf8(&out.stdout)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.strip_suffix(" missing"))
        .filter(|o| is_hex(o))
        .collect();
    if missing.is_empty() {
        return Ok(0);
    }
    let list = missing.join("\n") + "\n";
    // Pack them from the staged copy and let `index-pack` hash and store every object in the
    // repository's own store.
    let mut pack = git(common)
        .env("GIT_OBJECT_DIRECTORY", staging)
        .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", common.join("objects"))
        .args(["pack-objects", "--stdout", "-q"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let pack_out = pack.stdout.take().ok_or("no pack-objects stdout")?;
    let index = git(common)
        .args(["index-pack", "--stdin"])
        .stdin(Stdio::from(pack_out))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn();
    let mut stdin = pack.stdin.take();
    let w = std::thread::spawn(move || {
        if let Some(s) = stdin.as_mut() {
            let _ = s.write_all(list.as_bytes());
        }
    });
    let po = pack.wait_with_output();
    let _ = w.join();
    let io = index
        .map_err(|e| e.to_string())?
        .wait_with_output()
        .map_err(|e| e.to_string())?;
    let po = po.map_err(|e| e.to_string())?;
    if !po.status.success() {
        return Err(fail("pack-objects", &po));
    }
    if !io.status.success() {
        return Err(fail("index-pack", &io));
    }
    Ok(missing.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(dir: &Path, env: &[(&str, &Path)], args: &[&str]) -> String {
        let mut c = Command::new("git");
        c.arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES");
        for (k, v) in env {
            c.env(k, v);
        }
        let out = c.output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn host_has(repo: &Path, rev: &str) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["cat-file", "-e", rev])
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .status()
            .unwrap()
            .success()
    }

    /// 13 §6: objects a box writes to its private store reach the repository through the
    /// hardened host commands, and survive the deletion of the box's directory.
    #[test]
    fn host_git_imports_box_objects_and_keeps_them_after_release() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        g(&repo, &[], &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        g(&repo, &[], &["add", "-A"]);
        g(&repo, &[], &["commit", "-q", "-m", "base"]);
        let wt = root.join("wt");
        g(
            &repo,
            &[],
            &["worktree", "add", "-q", "-b", "vk/t", wt.to_str().unwrap()],
        );
        let sbx = root.join("sbx");
        let store = sbx.join(".git-objects");
        std::fs::create_dir_all(&store).unwrap();
        let host_objects = repo.join(".git/objects");
        let box_env: [(&str, &Path); 2] = [
            ("GIT_OBJECT_DIRECTORY", &store),
            ("GIT_ALTERNATE_OBJECT_DIRECTORIES", &host_objects),
        ];
        // The box commits twice and packs the first commit's objects.
        std::fs::write(wt.join("b.txt"), "box one\n").unwrap();
        g(&wt, &box_env, &["add", "-A"]);
        g(&wt, &box_env, &["commit", "-q", "-m", "box one"]);
        g(&wt, &box_env, &["repack", "-q", "-a", "-d", "-l"]);
        std::fs::write(wt.join("c.txt"), "box two\n").unwrap();
        g(&wt, &box_env, &["add", "-A"]);
        g(&wt, &box_env, &["commit", "-q", "-m", "box two"]);
        assert!(!host_has(&repo, "vk/t^{commit}"));

        // A planted symlink to another repository's object is never imported.
        let other = root.join("other");
        std::fs::create_dir_all(&other).unwrap();
        g(&other, &[], &["init", "-q"]);
        std::fs::write(other.join("secret"), "other repo secret\n").unwrap();
        let secret = g(&other, &[], &["hash-object", "-w", "secret"]);
        let (d, f) = secret.split_at(2);
        std::fs::create_dir_all(store.join(d)).unwrap();
        std::os::unix::fs::symlink(
            other.join(".git/objects").join(d).join(f),
            store.join(d).join(f),
        )
        .unwrap();

        register_box_objects(&wt, &store);
        // Any hardened host command in the repository (here: the main checkout) imports.
        let mut cmd = Command::new("git");
        crate::harden_command(&mut cmd, &repo).unwrap();
        assert!(host_has(&repo, "vk/t^{commit}"));
        assert!(host_has(&repo, "vk/t~1^{tree}"));
        assert!(!host_has(&repo, &secret), "a symlinked object was imported");
        assert_eq!(
            g(&repo, &[], &["log", "--format=%s", "-2", "vk/t"]),
            "box two\nbox one"
        );

        // A third commit after the last import arrives with the release.
        std::fs::write(wt.join("d.txt"), "box three\n").unwrap();
        g(&wt, &box_env, &["add", "-A"]);
        g(&wt, &box_env, &["commit", "-q", "-m", "box three"]);
        assert!(release_box_objects(&store).unwrap() >= 1);
        std::fs::remove_dir_all(&sbx).unwrap();
        assert_eq!(
            g(&repo, &[], &["log", "--format=%s", "-1", "vk/t"]),
            "box three"
        );
        g(
            &repo,
            &[],
            &["fsck", "--connectivity-only", "--no-dangling"],
        );
        // Forgotten: host git no longer looks for the store.
        assert!(
            stores()
                .lock()
                .unwrap()
                .iter()
                .all(|s| s.objects != canon(&store))
        );
    }

    #[test]
    fn common_dir_is_found_for_checkouts_worktrees_and_git_dirs() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        g(&repo, &[], &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        g(&repo, &[], &["add", "-A"]);
        g(&repo, &[], &["commit", "-q", "-m", "base"]);
        let wt = root.join("wt");
        g(
            &repo,
            &[],
            &["worktree", "add", "-q", "-b", "x", wt.to_str().unwrap()],
        );
        let common = repo.join(".git");
        assert_eq!(common_dir_of(&repo), Some(common.clone()));
        assert_eq!(common_dir_of(&repo.join("src")), Some(common.clone()));
        assert_eq!(common_dir_of(&wt), Some(common.clone()));
        assert_eq!(common_dir_of(&common), Some(common.clone()));
        assert_eq!(common_dir_of(&root), None);
    }
}
