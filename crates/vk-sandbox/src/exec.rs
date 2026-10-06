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
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;
    let _ = std::fs::remove_file(path);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(&serde_json::to_vec(spec).map_err(std::io::Error::other)?)?;
    Ok(())
}

/// Read and delete a spec file.
pub fn take_spec(path: &Path) -> std::io::Result<ExecSpec> {
    let bytes = std::fs::read(path)?;
    let _ = std::fs::remove_file(path);
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
    let policy: crate::policy::Policy = match std::fs::read(&policy_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
    {
        Some(p) => p,
        None => {
            eprintln!("vibeke sandbox: unreadable policy {policy_path}");
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
        "--policy".into(),
        policy_path,
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
    if let Some(policy) = get("--policy")
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice::<crate::policy::Policy>(&b).ok())
    {
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
