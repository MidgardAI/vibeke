//! Minimal process inspection: argv, executable path, cwd, children, start time.
//! Linux reads `/proc`; macOS uses `sysctl(KERN_PROCARGS2)` and libproc.

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProcInfo {
    pub pid: u32,
    pub ppid: u32,
    pub pgid: u32,
    pub argv: Vec<String>,
    pub exe: Option<String>,
    pub cwd: Option<String>,
    /// Process start time in an OS-specific unit; used with pid as a cache key.
    pub start: u64,
}

pub fn info(pid: u32) -> Option<ProcInfo> {
    imp::info(pid)
}

pub fn children(pid: u32) -> Vec<u32> {
    imp::children(pid)
}

pub fn argv(pid: u32) -> Vec<String> {
    imp::argv(pid)
}

pub fn cwd(pid: u32) -> Option<String> {
    imp::cwd(pid)
}

/// The environment of `pid` as `(name, value)` pairs. Best effort: empty when it can't be read
/// (another user's process; macOS, which withholds other processes' environments; or an
/// unsupported platform).
pub fn environ(pid: u32) -> Vec<(String, String)> {
    imp::environ(pid)
        .into_iter()
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            (!k.is_empty()).then(|| (k.to_string(), v.to_string()))
        })
        .collect()
}

/// The process tree below `pid` (inclusive), breadth first, up to `max_depth`.
pub fn tree(pid: u32, max_depth: usize) -> Vec<ProcInfo> {
    let mut out = Vec::new();
    let mut frontier = vec![(pid, 0usize)];
    while let Some((p, d)) = frontier.pop() {
        if let Some(i) = info(p) {
            out.push(i);
        }
        if d < max_depth {
            for c in children(p) {
                frontier.push((c, d + 1));
            }
        }
    }
    out
}

#[cfg(target_os = "linux")]
mod imp {
    use super::ProcInfo;
    use std::fs;

    pub fn argv(pid: u32) -> Vec<String> {
        fs::read(format!("/proc/{pid}/cmdline"))
            .map(|b| {
                b.split(|&c| c == 0)
                    .filter(|s| !s.is_empty())
                    .map(|s| String::from_utf8_lossy(s).into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn environ(pid: u32) -> Vec<String> {
        fs::read(format!("/proc/{pid}/environ"))
            .map(|b| {
                b.split(|&c| c == 0)
                    .filter(|s| !s.is_empty())
                    .map(|s| String::from_utf8_lossy(s).into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn cwd(pid: u32) -> Option<String> {
        fs::read_link(format!("/proc/{pid}/cwd"))
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
    }

    fn stat(pid: u32) -> Option<(u32, u32, u64)> {
        let s = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let rest = &s[s.rfind(')')? + 2..];
        let f: Vec<&str> = rest.split(' ').collect();
        // fields after comm: state(0) ppid(1) pgrp(2) ... starttime(19)
        Some((
            f.get(1)?.parse().ok()?,
            f.get(2)?.parse().ok()?,
            f.get(19)?.parse().ok()?,
        ))
    }

    pub fn info(pid: u32) -> Option<ProcInfo> {
        let (ppid, pgid, start) = stat(pid)?;
        Some(ProcInfo {
            pid,
            ppid,
            pgid,
            argv: argv(pid),
            exe: fs::read_link(format!("/proc/{pid}/exe"))
                .ok()
                .map(|p| p.to_string_lossy().into_owned()),
            cwd: cwd(pid),
            start,
        })
    }

    pub fn children(pid: u32) -> Vec<u32> {
        let mut out = Vec::new();
        if let Ok(tasks) = fs::read_dir(format!("/proc/{pid}/task")) {
            for t in tasks.flatten() {
                if let Ok(s) = fs::read_to_string(t.path().join("children")) {
                    out.extend(s.split_whitespace().filter_map(|x| x.parse::<u32>().ok()));
                }
            }
        }
        if out.is_empty() {
            // Kernels without CONFIG_PROC_CHILDREN: scan.
            if let Ok(rd) = fs::read_dir("/proc") {
                for e in rd.flatten() {
                    if let Some(p) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok())
                        && stat(p).is_some_and(|(pp, _, _)| pp == pid)
                    {
                        out.push(p);
                    }
                }
            }
        }
        out
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::ProcInfo;
    use std::ffi::CStr;

    pub fn argv(pid: u32) -> Vec<String> {
        procargs(pid).map(|(a, _)| a).unwrap_or_default()
    }

    pub fn environ(pid: u32) -> Vec<String> {
        procargs(pid).map(|(_, e)| e).unwrap_or_default()
    }

    /// `KERN_PROCARGS2`: argc, the exec path, NUL padding, argv, then the environment.
    fn procargs(pid: u32) -> Option<(Vec<String>, Vec<String>)> {
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
        let mut size: libc::size_t = 0;
        // SAFETY: sysctl with a null buffer queries the required size.
        if unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                std::ptr::null_mut(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        } != 0
        {
            return None;
        }
        let mut buf = vec![0u8; size];
        // SAFETY: buf has `size` bytes.
        if unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                buf.as_mut_ptr().cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        } != 0
        {
            return None;
        }
        buf.truncate(size);
        if buf.len() < 4 {
            return None;
        }
        let argc = i32::from_ne_bytes(buf[..4].try_into().unwrap()) as usize;
        let mut rest = &buf[4..];
        // exec path, then NUL padding
        let p = rest.iter().position(|&b| b == 0)?;
        rest = &rest[p..];
        let p = rest.iter().position(|&b| b != 0)?;
        rest = &rest[p..];
        let mut parts = rest.split(|&b| b == 0);
        let argv: Vec<String> = parts
            .by_ref()
            .take(argc)
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect();
        let env: Vec<String> = parts
            .take_while(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect();
        Some((argv, env))
    }

    pub fn cwd(pid: u32) -> Option<String> {
        let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
        // SAFETY: info is a properly sized out-buffer.
        let r = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                (&mut info as *mut libc::proc_vnodepathinfo).cast(),
                size,
            )
        };
        if r != size {
            return None;
        }
        // SAFETY: vip_path is a NUL-terminated C string buffer.
        let s = unsafe { CStr::from_ptr(info.pvi_cdir.vip_path.as_ptr().cast()) };
        Some(s.to_string_lossy().into_owned()).filter(|s| !s.is_empty())
    }

    fn exe(pid: u32) -> Option<String> {
        let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        // SAFETY: buf is PROC_PIDPATHINFO_MAXSIZE bytes.
        let n = unsafe {
            libc::proc_pidpath(
                pid as libc::c_int,
                buf.as_mut_ptr().cast(),
                buf.len() as u32,
            )
        };
        if n <= 0 {
            return None;
        }
        buf.truncate(n as usize);
        Some(String::from_utf8_lossy(&buf).into_owned())
    }

    pub fn info(pid: u32) -> Option<ProcInfo> {
        let mut bsd: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // SAFETY: bsd is a properly sized out-buffer.
        let r = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut bsd as *mut libc::proc_bsdinfo).cast(),
                size,
            )
        };
        if r != size {
            return None;
        }
        Some(ProcInfo {
            pid,
            ppid: bsd.pbi_ppid,
            pgid: bsd.pbi_pgid,
            argv: argv(pid),
            exe: exe(pid),
            cwd: cwd(pid),
            start: bsd.pbi_start_tvsec * 1_000_000 + bsd.pbi_start_tvusec,
        })
    }

    pub fn children(pid: u32) -> Vec<u32> {
        let mut buf = vec![0 as libc::pid_t; 256];
        // SAFETY: buffer size passed in bytes.
        let n = unsafe {
            libc::proc_listchildpids(
                pid as libc::pid_t,
                buf.as_mut_ptr().cast(),
                (buf.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int,
            )
        };
        if n <= 0 {
            return vec![];
        }
        buf.truncate(n as usize);
        buf.into_iter()
            .filter(|&p| p > 0)
            .map(|p| p as u32)
            .collect()
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod imp {
    use super::ProcInfo;
    pub fn argv(_: u32) -> Vec<String> {
        vec![]
    }
    pub fn cwd(_: u32) -> Option<String> {
        None
    }
    pub fn environ(_: u32) -> Vec<String> {
        vec![]
    }
    pub fn info(_: u32) -> Option<ProcInfo> {
        None
    }
    pub fn children(_: u32) -> Vec<u32> {
        vec![]
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn child_environ() {
        let mut c = std::process::Command::new("sleep")
            .arg("5")
            .env("VK_PROCINFO_PROBE", "a=b")
            .spawn()
            .unwrap();
        // `spawn` returns once the child's exec has started (the vfork parent is released, or
        // the CLOEXEC pipe closes, when the old mm is dropped), but Linux only records the new
        // image's argument and environment area a little later in the ELF loader; until then
        // `/proc/<pid>/environ` reads as empty. Real callers look at processes that are long past
        // exec (socket peers, pane children), so wait for the child to finish exec here.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let env = loop {
            let env = super::environ(c.id());
            // Between fork and exec the child still shows the parent's environment, so wait
            // for the probe itself rather than for any environment.
            let execed = env.iter().any(|(k, _)| k == "VK_PROCINFO_PROBE");
            if execed || !cfg!(target_os = "linux") || std::time::Instant::now() > deadline {
                break env;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        let _ = c.kill();
        let _ = c.wait();
        // macOS does not expose another process's environment (KERN_PROCARGS2 omits it);
        // there the reader returns nothing and callers fall back to process ancestry.
        if cfg!(target_os = "linux") {
            assert!(
                env.iter()
                    .any(|(k, v)| k == "VK_PROCINFO_PROBE" && v == "a=b"),
                "{env:?}"
            );
        }
    }

    #[test]
    fn self_info() {
        let me = std::process::id();
        let i = super::info(me).expect("info for self");
        assert!(!i.argv.is_empty());
        assert!(i.cwd.is_some());
    }
}
