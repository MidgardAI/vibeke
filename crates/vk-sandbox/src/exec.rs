//! `vibeke sandbox …` helper entry points. They run before any tokio runtime exists (like
//! `vibeke hold`) and end in `execve`.
//!
//! * `vibeke sandbox exec --spec <file>`: read a 0600 [`ExecSpec`] (deleting it first), then exec
//!   the command — under `sandbox-exec` when the spec carries a profile. Used when an isolated
//!   agent is launched into an existing pane: the typed command line names only the spec file,
//!   so credentials never appear on screen, in scrollback or in shell history.
//! * Linux only: `vibeke sandbox bwrap …` (outer: seccomp program on a pipe fd, then `bwrap`) and
//!   `vibeke sandbox inner …` (inside the namespace: Landlock, egress forwarder, exec).
//! * `vibeke sandbox box-init …`: PID 1 of a container box (static Linux binary mounted into
//!   the box): the egress forwarder plus zombie reaping.

use serde::{Deserialize, Serialize};
use std::ffi::CString;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ExecSpec {
    pub argv: Vec<String>,
    /// Replace the inherited env entirely (true for a sandboxed launch from a host pane).
    pub env_clear: bool,
    pub env: Vec<(String, String)>,
    /// Seatbelt profile to wrap the command with (macOS).
    pub profile: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
}

/// Write a spec file readable only by the owner.
pub fn write_spec(path: &Path, spec: &ExecSpec) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(spec).map_err(std::io::Error::other)?;
    crate::fsafe::write_nofollow(path, &bytes, 0o600)
}

fn untrusted(what: &str, p: &Path) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        format!("refusing spec {}: {what}", p.display()),
    )
}

/// Read and delete a spec file. The spec decides what runs (and whether it is sandboxed), so
/// it must be one Vibeke wrote: the parent directory is opened without following a symlink
/// and must be owned by this user and not group/world-writable; the file is opened relative
/// to it with `O_NOFOLLOW` and must be a regular file owned by this user with mode 0600 (no
/// group/other bits).
pub fn take_spec(path: &Path) -> std::io::Result<ExecSpec> {
    use rustix::fs::{self as rfs, Mode, OFlags};
    use std::io::Read as _;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let uid = rustix::process::geteuid().as_raw();
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(untrusted("not a file path", path));
    };
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let dir = rfs::open(
        parent,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let dir = std::fs::File::from(dir);
    let dm = dir.metadata()?;
    if dm.uid() != uid {
        return Err(untrusted("directory not owned by this user", path));
    }
    if dm.permissions().mode() & 0o022 != 0 {
        return Err(untrusted("directory is group- or world-writable", path));
    }
    let fd = rfs::openat(
        &dir,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )?;
    let mut f = std::fs::File::from(fd);
    let m = f.metadata()?;
    if !m.file_type().is_file() {
        return Err(untrusted("not a regular file", path));
    }
    if m.uid() != uid {
        return Err(untrusted("not owned by this user", path));
    }
    if m.permissions().mode() & 0o7777 != 0o600 {
        return Err(untrusted("mode is not 0600", path));
    }
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes)?;
    let _ = rfs::unlinkat(&dir, name, rfs::AtFlags::empty());
    serde_json::from_slice(&bytes).map_err(std::io::Error::other)
}

/// The argv/env that `exec` will hand to execve (pure; tested).
pub fn resolve(
    spec: &ExecSpec,
    inherited: &[(String, String)],
) -> (Vec<String>, Vec<(String, String)>) {
    let argv = match &spec.profile {
        Some(p) => crate::seatbelt::wrap_argv(p, &spec.argv),
        None => spec.argv.clone(),
    };
    let mut env: Vec<(String, String)> = if spec.env_clear {
        Vec::new()
    } else {
        inherited.to_vec()
    };
    for (k, v) in &spec.env {
        env.retain(|(x, _)| x != k);
        env.push((k.clone(), v.clone()));
    }
    (argv, env)
}

fn cstr(s: &str) -> CString {
    CString::new(s.replace('\0', "")).unwrap_or_default()
}

/// execvpe-style: search PATH from the *new* env for a bare command name.
fn find_in_path(cmd: &str, env: &[(String, String)]) -> String {
    if cmd.contains('/') {
        return cmd.to_string();
    }
    let path = env
        .iter()
        .find(|(k, _)| k == "PATH")
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| "/usr/bin:/bin".into());
    for d in path.split(':') {
        let p = Path::new(d).join(cmd);
        if p.is_file() {
            return p.to_string_lossy().into_owned();
        }
    }
    cmd.to_string()
}

/// Replace the current process. Returns only on failure.
pub fn exec(argv: &[String], env: &[(String, String)], cwd: Option<&Path>) -> std::io::Error {
    if argv.is_empty() {
        return std::io::Error::other("empty argv");
    }
    if let Some(c) = cwd
        && let Err(e) = std::env::set_current_dir(c)
    {
        return e;
    }
    let prog = cstr(&find_in_path(&argv[0], env));
    let args: Vec<CString> = argv.iter().map(|a| cstr(a)).collect();
    let envs: Vec<CString> = env.iter().map(|(k, v)| cstr(&format!("{k}={v}"))).collect();
    let mut ap: Vec<*const libc::c_char> = args.iter().map(|a| a.as_ptr()).collect();
    ap.push(std::ptr::null());
    let mut ep: Vec<*const libc::c_char> = envs.iter().map(|a| a.as_ptr()).collect();
    ep.push(std::ptr::null());
    // SAFETY: NUL-terminated arrays of valid C strings that outlive the call.
    unsafe {
        libc::execve(prog.as_ptr(), ap.as_ptr(), ep.as_ptr());
    }
    std::io::Error::last_os_error()
}

/// `vibeke sandbox <sub> …`. Returns an exit code on failure (success never returns).
pub fn main(args: &[String]) -> i32 {
    let get = |k: &str| {
        args.iter()
            .position(|a| a == k)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    match args.first().map(String::as_str) {
        Some("exec") => {
            let Some(spec) = get("--spec") else {
                eprintln!("vibeke sandbox exec --spec FILE");
                return 2;
            };
            let spec = match take_spec(Path::new(&spec)) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("vibeke sandbox exec: {e}");
                    return 1;
                }
            };
            let inherited: Vec<(String, String)> = std::env::vars().collect();
            let (argv, env) = resolve(&spec, &inherited);
            let e = exec(&argv, &env, spec.cwd.as_deref());
            eprintln!("vibeke sandbox exec: {}: {e}", argv[0]);
            127
        }
        Some("box-init") => box_init(args),
        #[cfg(target_os = "linux")]
        Some("bwrap") => linux_outer(args),
        #[cfg(target_os = "linux")]
        Some("inner") => linux_inner(args),
        _ => {
            eprintln!("vibeke sandbox exec --spec FILE");
            2
        }
    }
}

#[cfg(target_os = "linux")]
fn after_dashdash(args: &[String]) -> Vec<String> {
    args.iter()
        .position(|a| a == "--")
        .map(|i| args[i + 1..].to_vec())
        .unwrap_or_default()
}

#[cfg(target_os = "linux")]
fn linux_outer(args: &[String]) -> i32 {
    use crate::linux;
    let get = |k: &str| {
        args.iter()
            .position(|a| a == k)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let Some(policy_path) = get("--policy") else {
        return 2;
    };
    let policy_bytes = match std::fs::read(&policy_path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("vibeke sandbox: unreadable policy {policy_path}: {e}");
            return 1;
        }
    };
    let policy: crate::policy::Policy = match serde_json::from_slice(&policy_bytes) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("vibeke sandbox: invalid policy {policy_path}: {e}");
            return 1;
        }
    };
    // The policy lives in Vibeke's control dir, which the box can neither read nor write: hand
    // it to the inner helper on an inherited memfd instead of a path.
    let policy_fd = match policy_memfd(&policy_bytes) {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("vibeke sandbox: policy memfd: {e}");
            return 1;
        }
    };
    let cwd = PathBuf::from(get("--cwd").unwrap_or_else(|| "/".into()));
    let egress = get("--egress").map(PathBuf::from);
    let cmd = after_dashdash(args);
    let Some(arch) = linux::Arch::host() else {
        return 1;
    };
    let bpf = linux::seccomp_bytes(&linux::seccomp_program(arch));
    let mut fds = [0i32; 2];
    // SAFETY: pipe with a valid 2-int array; the write end is closed after writing.
    unsafe {
        if libc::pipe(fds.as_mut_ptr()) != 0 {
            return 1;
        }
        libc::write(fds[1], bpf.as_ptr().cast(), bpf.len());
        libc::close(fds[1]);
    }
    let self_exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "vibeke".into());
    let mut argv = vec![linux::BWRAP.to_string()];
    argv.extend(linux::bwrap_args(
        &policy,
        &cwd,
        egress.as_deref(),
        Some(fds[0]),
    ));
    argv.extend([
        "--".into(),
        self_exe,
        "sandbox".into(),
        "inner".into(),
        "--policy-fd".into(),
        policy_fd.to_string(),
    ]);
    if let Some(p) = get("--proxy-port") {
        argv.extend(["--proxy-port".into(), p]);
    }
    argv.push("--".into());
    argv.extend(cmd);
    let env: Vec<(String, String)> = std::env::vars().collect();
    let e = exec(&argv, &env, None);
    eprintln!("vibeke sandbox: bwrap: {e}");
    127
}

/// An anonymous in-memory file holding `bytes`, inherited across exec (no `CLOEXEC`) and
/// rewound so the inner helper reads it from the start.
#[cfg(target_os = "linux")]
fn policy_memfd(bytes: &[u8]) -> std::io::Result<i32> {
    use std::io::{Seek as _, Write as _};
    use std::os::fd::{FromRawFd as _, IntoRawFd as _};
    // SAFETY: a NUL-terminated name and no flags (the fd must survive execve).
    let fd = unsafe { libc::memfd_create(c"vibeke-policy".as_ptr(), 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` is a freshly created descriptor we own.
    let mut f = unsafe { std::fs::File::from_raw_fd(fd) };
    f.write_all(bytes)?;
    f.rewind()?;
    Ok(f.into_raw_fd())
}

#[cfg(target_os = "linux")]
fn read_policy_fd(fd: &str) -> Result<Vec<u8>, String> {
    use std::io::{Read as _, Seek as _};
    use std::os::fd::FromRawFd as _;
    let fd: i32 = fd.parse().map_err(|_| format!("bad --policy-fd {fd}"))?;
    if fd < 3 {
        return Err(format!("bad --policy-fd {fd}"));
    }
    // SAFETY: the outer helper passed us this inherited descriptor; we take ownership and
    // close it before exec so the contained command never sees the policy.
    let mut f = unsafe { std::fs::File::from_raw_fd(fd) };
    let _ = f.rewind();
    let mut b = Vec::new();
    f.read_to_end(&mut b).map_err(|e| e.to_string())?;
    Ok(b)
}

#[cfg(target_os = "linux")]
fn linux_inner(args: &[String]) -> i32 {
    use crate::linux;
    let get = |k: &str| {
        args.iter()
            .position(|a| a == k)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let cmd = after_dashdash(args);
    let bytes = if let Some(fd) = get("--policy-fd") {
        Some(read_policy_fd(&fd))
    } else {
        get("--policy").map(|p| std::fs::read(p).map_err(|e| e.to_string()))
    };
    if let Some(bytes) = bytes {
        // Asked for a policy: an unreadable one fails closed rather than skipping Landlock.
        let policy = match bytes.and_then(|b| {
            serde_json::from_slice::<crate::policy::Policy>(&b).map_err(|e| e.to_string())
        }) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("vibeke sandbox: unreadable policy: {e}");
                return 1;
            }
        };
        match linux::apply_landlock(&linux::landlock_rules(&policy)) {
            Ok(true) => {}
            Ok(false) => eprintln!("vibeke sandbox: Landlock unavailable; mount namespace only"),
            Err(e) => eprintln!("vibeke sandbox: Landlock: {e}"),
        }
    }
    if let Some(port) = get("--proxy-port").and_then(|p| p.parse::<u16>().ok()) {
        // SAFETY: single-threaded here (no runtime yet); the child only runs the forwarder.
        if unsafe { libc::fork() } == 0 {
            forwarder(("127.0.0.1", port), crate::linux::EGRESS_SOCKET_IN_BOX);
            std::process::exit(0);
        }
    }
    let env: Vec<(String, String)> = std::env::vars().collect();
    let e = exec(&cmd, &env, None);
    eprintln!("vibeke sandbox: exec: {e}");
    127
}

/// `<addr>` inside the box's net namespace → the host proxy's unix socket (Linux sandbox and
/// the container box). Blocks forever accepting connections.
pub fn forwarder(addr: impl std::net::ToSocketAddrs, sock: &str) {
    let Ok(l) = std::net::TcpListener::bind(addr) else {
        return;
    };
    serve_forwarder(l, sock.to_string());
}

extern "C" fn box_init_term(_: libc::c_int) {
    // SAFETY: async-signal-safe exit.
    unsafe { libc::_exit(0) }
}

/// `vibeke sandbox box-init [--proxy-port P --egress SOCK]`: PID 1 of a container box (13 §4,
/// §7). Reaps orphaned children and exits on SIGTERM/SIGINT; with `--proxy-port P --egress SOCK`
/// it also forwards `127.0.0.1:P` to a unix socket (the box link does not need this).
fn box_init(args: &[String]) -> i32 {
    let get = |k: &str| {
        args.iter()
            .position(|a| a == k)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let handler = box_init_term as extern "C" fn(libc::c_int);
    // SAFETY: installing a handler that only calls _exit.
    unsafe {
        libc::signal(libc::SIGTERM, handler as libc::sighandler_t);
        libc::signal(libc::SIGINT, handler as libc::sighandler_t);
    }
    if let (Some(port), Some(sock)) = (
        get("--proxy-port").and_then(|p| p.parse::<u16>().ok()),
        get("--egress"),
    ) {
        std::thread::spawn(move || forwarder(("127.0.0.1", port), &sock));
    }
    loop {
        let mut st = 0;
        // SAFETY: plain waitpid on any child.
        if unsafe { libc::waitpid(-1, &mut st, 0) } < 0 {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
}

/// Accept loop of [`forwarder`] (split out so tests can bind an ephemeral port first).
pub fn serve_forwarder(l: std::net::TcpListener, sock: String) {
    use std::io::copy;
    use std::os::unix::net::UnixStream;
    for c in l.incoming().flatten() {
        let Ok(u) = UnixStream::connect(&sock) else {
            continue;
        };
        let (mut c2, mut u2) = match (c.try_clone(), u.try_clone()) {
            (Ok(a), Ok(b)) => (a, b),
            _ => continue,
        };
        let (mut c, mut u) = (c, u);
        std::thread::spawn(move || {
            let _ = copy(&mut c2, &mut u2);
            let _ = u2.shutdown(std::net::Shutdown::Write);
        });
        std::thread::spawn(move || {
            let _ = copy(&mut u, &mut c);
            let _ = c.shutdown(std::net::Shutdown::Write);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_roundtrip_is_private_and_consumed() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("spec.json");
        let spec = ExecSpec {
            argv: vec!["claude".into()],
            env_clear: true,
            env: vec![("CLAUDE_CODE_OAUTH_TOKEN".into(), "FAKE".into())],
            profile: Some("/x/profile.sb".into()),
            cwd: None,
        };
        write_spec(&p, &spec).unwrap();
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let back = take_spec(&p).unwrap();
        assert!(!p.exists());
        assert_eq!(back, spec);
        let (argv, env) = resolve(&back, &[("SECRET".into(), "x".into())]);
        assert_eq!(argv[0], crate::seatbelt::SANDBOX_EXEC);
        assert_eq!(argv.last().unwrap(), "claude");
        assert_eq!(
            env,
            [("CLAUDE_CODE_OAUTH_TOKEN".to_string(), "FAKE".to_string())]
        );
    }

    #[test]
    fn take_spec_refuses_tampered_or_foreign_files() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().canonicalize().unwrap().join("ctl");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let spec = ExecSpec {
            argv: vec!["sh".into()],
            ..Default::default()
        };
        // Loose file mode.
        let p = dir.join("a.json");
        write_spec(&p, &spec).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(take_spec(&p).is_err());
        assert!(p.exists(), "a refused spec is not consumed");
        // A symlink to an otherwise valid spec.
        let real = dir.join("real.json");
        write_spec(&real, &spec).unwrap();
        let link = dir.join("link.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(take_spec(&link).is_err());
        // Not a regular file.
        let fifo_dir = dir.join("sub");
        std::fs::create_dir(&fifo_dir).unwrap();
        assert!(take_spec(&fifo_dir).is_err());
        // A group/world-writable parent directory.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(take_spec(&real).is_err());
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        // A symlinked parent directory.
        let dlink = t.path().join("dlink");
        std::os::unix::fs::symlink(&dir, &dlink).unwrap();
        assert!(take_spec(&dlink.join("real.json")).is_err());
        // The genuine article.
        assert_eq!(take_spec(&real).unwrap(), spec);
        assert!(!real.exists());
    }

    #[test]
    fn forwarder_relays_to_the_unix_socket() {
        use std::io::{Read, Write};
        let t = tempfile::tempdir().unwrap();
        let sock = t.path().join("egress.sock");
        let ul = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        std::thread::spawn(move || {
            for mut s in ul.incoming().flatten() {
                let mut b = [0u8; 4];
                s.read_exact(&mut b).unwrap();
                s.write_all(&b).unwrap();
                s.write_all(b"!").unwrap();
            }
        });
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let sp = sock.to_string_lossy().into_owned();
        std::thread::spawn(move || serve_forwarder(l, sp));
        let mut c = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        c.write_all(b"ping").unwrap();
        let mut out = [0u8; 5];
        c.read_exact(&mut out).unwrap();
        assert_eq!(&out, b"ping!");
    }

    #[test]
    fn env_merge_without_clear() {
        let spec = ExecSpec {
            argv: vec!["codex".into()],
            env: vec![("CODEX_HOME".into(), "/p/codex".into())],
            ..Default::default()
        };
        let (argv, env) = resolve(
            &spec,
            &[
                ("PATH".into(), "/bin".into()),
                ("CODEX_HOME".into(), "/old".into()),
            ],
        );
        assert_eq!(argv, ["codex"]);
        assert_eq!(
            env,
            [
                ("PATH".to_string(), "/bin".to_string()),
                ("CODEX_HOME".to_string(), "/p/codex".to_string())
            ]
        );
    }
}
