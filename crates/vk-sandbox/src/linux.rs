//! Linux backend (13 §2.1 `sandbox`): bubblewrap mount/PID/net namespaces + Landlock
//! filesystem rules + a seccomp deny-list, rendered from the same [`Policy`] as Seatbelt.
//!
//! The generators below are plain functions compiled on every platform so they are unit-tested
//! on the macOS development machine; only [`apply_landlock`] and the in-namespace egress
//! forwarder touch Linux syscalls and are `cfg(target_os = "linux")`. **Unverified on a real
//! Linux host** (spec 13 implementation status).
//!
//! Launch chain on Linux:
//! `bwrap <args> --seccomp <fd> -- vibeke sandbox inner --spec <file>` → the inner helper
//! applies Landlock (it must run *after* bwrap has built the mount namespace: Landlock forbids
//! mount changes), starts the 127.0.0.1:<port> → unix-socket egress forwarder (the proxy lives
//! on the host; a fresh net namespace has only loopback), then execs the pane command.

use crate::policy::Policy;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const BWRAP: &str = "bwrap";
/// Where the host egress proxy's unix socket is bound inside the namespace.
pub const EGRESS_SOCKET_IN_BOX: &str = "/run/vibeke/egress.sock";

/// bubblewrap arguments (without the trailing `-- <cmd>`). `egress_socket` is the host path of
/// the proxy's unix listener; `seccomp_fd` the fd number carrying the BPF program.
pub fn bwrap_args(
    p: &Policy,
    cwd: &Path,
    egress_socket: Option<&Path>,
    seccomp_fd: Option<i32>,
) -> Vec<String> {
    let s = |p: &Path| p.to_string_lossy().into_owned();
    let mut a: Vec<String> = vec![
        "--die-with-parent".into(),
        "--unshare-user-try".into(),
        "--unshare-pid".into(),
        "--unshare-ipc".into(),
        "--unshare-uts".into(),
        "--unshare-cgroup-try".into(),
        "--unshare-net".into(),
        // The whole host filesystem read-only, then hide and re-expose.
        "--ro-bind".into(),
        "/".into(),
        "/".into(),
        "--dev".into(),
        "/dev".into(),
        "--proc".into(),
        "/proc".into(),
        "--tmpfs".into(),
        "/tmp".into(),
        "--tmpfs".into(),
        "/var/tmp".into(),
    ];
    // Layer 1: hide home and other deny roots behind empty tmpfs mounts.
    for d in &p.deny_read {
        if d == Path::new("/private/tmp") || d == Path::new("/private/var/tmp") {
            continue; // macOS spellings; /tmp is already a tmpfs here
        }
        a.extend(["--tmpfs".into(), s(d)]);
    }
    // Layer 2: re-expose the allowlist read-only.
    for r in &p.allow_read {
        a.extend(["--ro-bind-try".into(), s(r), s(r)]);
    }
    // Layer 2b: hidden Vibeke dirs, then the specific re-allows under them.
    for h in &p.hidden {
        a.extend(["--tmpfs".into(), s(h)]);
    }
    for r in &p.allow_read_late {
        a.extend(["--ro-bind-try".into(), s(r), s(r)]);
    }
    for n in &p.never_read {
        // Only mask what exists; masking a missing path would create it on the tmpfs.
        if n.exists() {
            if n.is_dir() {
                a.extend(["--tmpfs".into(), s(n)]);
            } else {
                a.extend(["--ro-bind".into(), "/dev/null".into(), s(n)]);
            }
        }
    }
    // Layer 3: writable binds.
    for w in &p.allow_write {
        a.extend(["--bind-try".into(), s(w), s(w)]);
    }
    // Git: re-bind the git dirs read-only, then the writable parts.
    for g in &p.deny_write_git {
        a.extend(["--ro-bind-try".into(), s(g), s(g)]);
    }
    for g in &p.allow_write_git {
        a.extend(["--bind-try".into(), s(g), s(g)]);
    }
    // Final: never writable.
    for d in p.deny_write.iter().chain(p.deny_write_literal.iter()) {
        if d == &p.checkout {
            continue; // the root's own entry is protected by the parent being read-only
        }
        a.extend(["--ro-bind-try".into(), s(d), s(d)]);
    }
    for u in &p.unix_sockets {
        a.extend(["--bind-try".into(), s(u), s(u)]);
    }
    if let Some(sock) = egress_socket {
        a.extend(["--bind".into(), s(sock), EGRESS_SOCKET_IN_BOX.into()]);
    }
    if let Some(fd) = seccomp_fd {
        a.extend(["--seccomp".into(), fd.to_string()]);
    }
    a.extend(["--chdir".into(), s(cwd)]);
    a
}

/// Landlock access rights (ABI v1 subset) per path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LandlockRule {
    pub path: PathBuf,
    pub write: bool,
}

/// Landlock rules that mirror the policy *after* bwrap built the namespace: read+exec on the
/// system and the read allowlists, read-write on the write allowlists. Landlock cannot express
/// "deny under an allowed parent", so the git and credential carve-outs rely on the read-only
/// bind mounts from [`bwrap_args`].
pub fn landlock_rules(p: &Policy) -> Vec<LandlockRule> {
    let mut v: Vec<LandlockRule> = [
        "/usr", "/bin", "/sbin", "/lib", "/lib64", "/etc", "/opt", "/nix", "/proc", "/sys", "/run",
    ]
    .iter()
    .map(|d| LandlockRule {
        path: PathBuf::from(d),
        write: false,
    })
    .collect();
    v.push(LandlockRule {
        path: "/dev".into(),
        write: true,
    });
    v.push(LandlockRule {
        path: "/tmp".into(),
        write: true,
    });
    for r in p.allow_read.iter().chain(p.allow_read_late.iter()) {
        v.push(LandlockRule {
            path: r.clone(),
            write: false,
        });
    }
    for w in p.allow_write.iter().chain(p.allow_write_git.iter()) {
        v.push(LandlockRule {
            path: w.clone(),
            write: true,
        });
    }
    v
}

/// Syscalls refused with EPERM inside the box: tracing other processes, kernel keyrings,
/// namespace/mount manipulation, kernel modules, BPF and perf.
pub const SECCOMP_DENY: &[&str] = &[
    "ptrace",
    "process_vm_readv",
    "process_vm_writev",
    "mount",
    "umount2",
    "pivot_root",
    "setns",
    "unshare",
    "keyctl",
    "add_key",
    "request_key",
    "bpf",
    "perf_event_open",
    "userfaultfd",
    "kexec_load",
    "init_module",
    "finit_module",
    "delete_module",
    "open_by_handle_at",
    "name_to_handle_at",
];

/// Syscall numbers for x86_64 and aarch64 (asm-generic).
pub fn syscall_nr(name: &str, arch: Arch) -> Option<u32> {
    let (x86, arm) = match name {
        "ptrace" => (101, 117),
        "process_vm_readv" => (310, 270),
        "process_vm_writev" => (311, 271),
        "mount" => (165, 40),
        "umount2" => (166, 39),
        "pivot_root" => (155, 41),
        "setns" => (308, 268),
        "unshare" => (272, 97),
        "keyctl" => (250, 219),
        "add_key" => (248, 217),
        "request_key" => (249, 218),
        "bpf" => (321, 280),
        "perf_event_open" => (298, 241),
        "userfaultfd" => (323, 282),
        "kexec_load" => (246, 104),
        "init_module" => (175, 105),
        "finit_module" => (313, 273),
        "delete_module" => (176, 106),
        "open_by_handle_at" => (304, 265),
        "name_to_handle_at" => (303, 264),
        "ioctl" => (16, 29),
        _ => return None,
    };
    Some(match arch {
        Arch::X86_64 => x86,
        Arch::Aarch64 => arm,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    X86_64,
    Aarch64,
}

impl Arch {
    pub fn audit(&self) -> u32 {
        match self {
            Arch::X86_64 => 0xc000_003e,  // AUDIT_ARCH_X86_64
            Arch::Aarch64 => 0xc000_00b7, // AUDIT_ARCH_AARCH64
        }
    }
    pub fn host() -> Option<Arch> {
        if cfg!(target_arch = "x86_64") {
            Some(Arch::X86_64)
        } else if cfg!(target_arch = "aarch64") {
            Some(Arch::Aarch64)
        } else {
            None
        }
    }
}

/// One classic-BPF instruction (`struct sock_filter`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SockFilter {
    pub code: u16,
    pub jt: u8,
    pub jf: u8,
    pub k: u32,
}

const BPF_LD_W_ABS: u16 = 0x20;
const BPF_JEQ_K: u16 = 0x15;
const BPF_JGE_K: u16 = 0x35;
const BPF_RET_K: u16 = 0x06;
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
const EPERM: u32 = 1;
const TIOCSTI: u32 = 0x5412;

/// Build the seccomp filter: wrong arch → kill; each denied syscall → EPERM; `ioctl(TIOCSTI)`
/// (terminal input injection) → EPERM; everything else allowed.
pub fn seccomp_program(arch: Arch) -> Vec<SockFilter> {
    let ins = |code, jt, jf, k| SockFilter { code, jt, jf, k };
    let mut p = vec![
        ins(BPF_LD_W_ABS, 0, 0, 4), // seccomp_data.arch
        ins(BPF_JEQ_K, 1, 0, arch.audit()),
        ins(BPF_RET_K, 0, 0, SECCOMP_RET_KILL_PROCESS),
        ins(BPF_LD_W_ABS, 0, 0, 0), // seccomp_data.nr
    ];
    if arch == Arch::X86_64 {
        // x32 ABI syscall numbers (bit 30) would bypass the number checks.
        p.push(ins(BPF_JGE_K, 0, 1, 0x4000_0000));
        p.push(ins(BPF_RET_K, 0, 0, SECCOMP_RET_KILL_PROCESS));
    }
    for name in SECCOMP_DENY {
        if let Some(nr) = syscall_nr(name, arch) {
            p.push(ins(BPF_JEQ_K, 0, 1, nr));
            p.push(ins(BPF_RET_K, 0, 0, SECCOMP_RET_ERRNO | EPERM));
        }
    }
    // ioctl(fd, TIOCSTI, …): args[1] low word at offset 16 + 8.
    let ioctl = syscall_nr("ioctl", arch).unwrap_or(u32::MAX);
    p.push(ins(BPF_JEQ_K, 0, 3, ioctl));
    p.push(ins(BPF_LD_W_ABS, 0, 0, 24));
    p.push(ins(BPF_JEQ_K, 0, 1, TIOCSTI));
    p.push(ins(BPF_RET_K, 0, 0, SECCOMP_RET_ERRNO | EPERM));
    p.push(ins(BPF_RET_K, 0, 0, SECCOMP_RET_ALLOW));
    p
}

/// Serialize the program the way `bwrap --seccomp <fd>` reads it (native-endian
/// `struct sock_filter` array).
pub fn seccomp_bytes(prog: &[SockFilter]) -> Vec<u8> {
    let mut out = Vec::with_capacity(prog.len() * 8);
    for i in prog {
        out.extend_from_slice(&i.code.to_ne_bytes());
        out.push(i.jt);
        out.push(i.jf);
        out.extend_from_slice(&i.k.to_ne_bytes());
    }
    out
}

/// Is bubblewrap installed (`vibeke doctor`)?
pub fn bwrap_available() -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|d| d.join(BWRAP).is_file()))
}

/// Apply Landlock rules to the current process (inner helper, after bwrap). Best effort: on a
/// kernel without Landlock the bwrap mount namespace remains the filesystem boundary.
#[cfg(target_os = "linux")]
pub fn apply_landlock(rules: &[LandlockRule]) -> std::io::Result<bool> {
    use std::os::unix::ffi::OsStrExt;
    const CREATE_RULESET: libc::c_long = 444;
    const ADD_RULE: libc::c_long = 445;
    const RESTRICT_SELF: libc::c_long = 446;
    const RULE_PATH_BENEATH: libc::c_int = 1;
    // ABI v1 access rights.
    const EXECUTE: u64 = 1 << 0;
    const WRITE_FILE: u64 = 1 << 1;
    const READ_FILE: u64 = 1 << 2;
    const READ_DIR: u64 = 1 << 3;
    const REMOVE_DIR: u64 = 1 << 4;
    const REMOVE_FILE: u64 = 1 << 5;
    const MAKE_CHAR: u64 = 1 << 6;
    const MAKE_DIR: u64 = 1 << 7;
    const MAKE_REG: u64 = 1 << 8;
    const MAKE_SOCK: u64 = 1 << 9;
    const MAKE_FIFO: u64 = 1 << 10;
    const MAKE_BLOCK: u64 = 1 << 11;
    const MAKE_SYM: u64 = 1 << 12;
    const READ: u64 = EXECUTE | READ_FILE | READ_DIR;
    const WRITE: u64 = WRITE_FILE
        | REMOVE_DIR
        | REMOVE_FILE
        | MAKE_CHAR
        | MAKE_DIR
        | MAKE_REG
        | MAKE_SOCK
        | MAKE_FIFO
        | MAKE_BLOCK
        | MAKE_SYM;
    #[repr(C)]
    struct RulesetAttr {
        handled_access_fs: u64,
    }
    #[repr(C, packed)]
    struct PathBeneath {
        allowed_access: u64,
        parent_fd: i32,
    }
    let attr = RulesetAttr {
        handled_access_fs: READ | WRITE,
    };
    // SAFETY: raw syscalls with valid pointers/sizes; fds are closed below.
    unsafe {
        let fd = libc::syscall(
            CREATE_RULESET,
            &attr as *const RulesetAttr,
            std::mem::size_of::<RulesetAttr>(),
            0u32,
        );
        if fd < 0 {
            return Ok(false);
        }
        let fd = fd as i32;
        for r in rules {
            let c = match std::ffi::CString::new(r.path.as_os_str().as_bytes()) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let pfd = libc::open(c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC);
            if pfd < 0 {
                continue;
            }
            let pb = PathBeneath {
                allowed_access: if r.write { READ | WRITE } else { READ },
                parent_fd: pfd,
            };
            libc::syscall(
                ADD_RULE,
                fd,
                RULE_PATH_BENEATH,
                &pb as *const PathBeneath,
                0u32,
            );
            libc::close(pfd);
        }
        if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
            libc::close(fd);
            return Err(std::io::Error::last_os_error());
        }
        let r = libc::syscall(RESTRICT_SELF, fd, 0u32);
        libc::close(fd);
        Ok(r == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::tests::spec;

    fn pos(a: &[String], needle: &[&str]) -> Option<usize> {
        a.windows(needle.len())
            .position(|w| w.iter().zip(needle).all(|(x, y)| x == y))
    }

    #[test]
    fn bwrap_args_layering() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        let pol = Policy::from_spec(&spec(&root));
        let cwd = root.join("home/code/repo-task");
        let a = bwrap_args(
            &pol,
            &cwd,
            Some(Path::new("/run/user/1/vk/egress.sock")),
            Some(9),
        );
        let home = root.join("home").to_string_lossy().into_owned();
        let co = cwd.to_string_lossy().into_owned();
        let state = root.join("state").to_string_lossy().into_owned();
        let inbox = root.join("state/inbox").to_string_lossy().into_owned();
        assert!(a.contains(&"--unshare-net".to_string()));
        let hide_home = pos(&a, &["--tmpfs", &home]).unwrap();
        let bind_co = pos(&a, &["--bind-try", &co, &co]).unwrap();
        let hide_state = pos(&a, &["--tmpfs", &state]).unwrap();
        let show_inbox = pos(&a, &["--ro-bind-try", &inbox, &inbox]).unwrap();
        assert!(hide_home < hide_state && hide_state < show_inbox && show_inbox < bind_co);
        let hooks = root
            .join("home/code/repo/.git/hooks")
            .to_string_lossy()
            .into_owned();
        let ro_hooks = pos(&a, &["--ro-bind-try", &hooks, &hooks]).unwrap();
        assert!(ro_hooks > bind_co);
        assert!(
            pos(
                &a,
                &["--bind", "/run/user/1/vk/egress.sock", EGRESS_SOCKET_IN_BOX]
            )
            .is_some()
        );
        assert!(pos(&a, &["--seccomp", "9"]).is_some());
        assert_eq!(a[a.len() - 2..], ["--chdir".to_string(), co]);
    }

    #[test]
    fn landlock_rules_cover_allowlists() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        let pol = Policy::from_spec(&spec(&root));
        let r = landlock_rules(&pol);
        assert!(r.iter().any(|x| x.path == Path::new("/usr") && !x.write));
        assert!(
            r.iter()
                .any(|x| x.path == root.join("home/code/repo-task") && x.write)
        );
        assert!(
            r.iter()
                .any(|x| x.path == root.join("state/inbox") && !x.write)
        );
        assert!(!r.iter().any(|x| x.path == root.join("home")));
    }

    #[test]
    fn seccomp_program_shape() {
        for arch in [Arch::X86_64, Arch::Aarch64] {
            let p = seccomp_program(arch);
            assert_eq!(p[1].k, arch.audit());
            assert_eq!(p.last().unwrap().k, SECCOMP_RET_ALLOW);
            let ptrace = syscall_nr("ptrace", arch).unwrap();
            let i = p
                .iter()
                .position(|x| x.code == BPF_JEQ_K && x.k == ptrace)
                .unwrap();
            assert_eq!(p[i + 1].k, SECCOMP_RET_ERRNO | EPERM);
            // Every jump target stays inside the program.
            for (n, x) in p.iter().enumerate() {
                if x.code == BPF_JEQ_K || x.code == BPF_JGE_K {
                    assert!(n + 1 + (x.jt.max(x.jf) as usize) < p.len());
                }
            }
            assert_eq!(seccomp_bytes(&p).len(), p.len() * 8);
            assert!(p.iter().any(|x| x.k == TIOCSTI));
        }
        for n in SECCOMP_DENY {
            assert!(syscall_nr(n, Arch::X86_64).is_some(), "{n}");
        }
    }
}
